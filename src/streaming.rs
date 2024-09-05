//! This module provides the "glue" between.

/// This is the message type received by our camera thread.
#[derive(Debug)]
pub enum CameraControl {
  /// A graceful shutdown.
  Terminate,
  /// Sent by the network thread when the connection with kvs is established.
  NetworkReady,
}

/// This is the message type received by our network stream thread.
#[derive(Debug)]
pub enum StreamControl {
  /// A graceful shutdown.
  Terminate,
  /// Received (sent by the camera) whenever we have image data to handle.
  DataAvailable(crate::camera::JpegFrame),
}

pub mod network {
  use super::{CameraControl, StreamControl};
  use crate::{camera, kvs, mkv};
  use anyhow::Context;
  use std::io;

  /// This number is the amount of frames we keep before collapsing our current mkv stack into a
  /// vector and shipping those bytes off to KVS. In an ideal world this would be exposed through
  /// some configuration.
  const MAX_BUFFERED_FRAMES: usize = 60;

  /// This type is used to provide an api that will take (m)jpeg data coming into our network
  /// thread and write it to the open network connection.
  struct NetworkedVideoEncoder {
    /// The moment we started the entire segment. This is effectively the timestamp of the very
    /// first frame of image data we have ever received.
    segment_start: Option<std::time::Instant>,
    /// While streaming, we want to keep track of how long we have let our current cluster remain
    /// open. This duration, in addition to the frames per second, affect the amount of data we
    /// buffer before dumping our data over the wire.
    current_cluster_start: Option<std::time::Instant>,
    /// The amount of frames we have written to our cursor since we last dumped it to the
    /// writer/destination.
    written_frames: usize,
    /// The underlying video encoder.
    encoder: openh264::encoder::Encoder,
    /// Our MKV container wrapper.
    mkv_cursor: mkv::Cursor,
  }

  impl NetworkedVideoEncoder {
    /// Constructs our encoder, returning an `Err` if the openh264 library is unable to
    /// initialize successfully.
    fn new() -> anyhow::Result<Self> {
      let encoder =
        openh264::encoder::Encoder::new().with_context(|| "unable to initialize the openh264 encoder")?;

      let mut mkv_cursor = mkv::Cursor::default();
      mkv_cursor
        .start()
        .with_context(|| "unable to write starting blocks to container")?;

      Ok(Self {
        encoder,
        current_cluster_start: None,
        segment_start: None,
        written_frames: 0,
        mkv_cursor,
      })
    }

    /// This method is responsible for using the h264 encoder and container writer to produce data
    /// that we can actually send over the wire.
    async fn encode_to<W>(&mut self, frame: &camera::JpegFrame, destination: W) -> anyhow::Result<()>
    where
      W: async_std::io::Write + std::marker::Unpin,
    {
      let mut rgb_decoder = jpeg_decoder::Decoder::new(io::Cursor::new(&frame.data));
      let rgb_data = rgb_decoder.decode().with_context(|| {
        format!(
          "unable to extract raw rgb data from input of {} bytes",
          frame.len()
        )
      })?;

      let rgb = openh264::formats::RgbSliceU8::new(
        &rgb_data,
        (frame.dimensions.0 as usize, frame.dimensions.1 as usize),
      );
      let yuv_buffer = openh264::formats::YUVBuffer::from_rgb_source(rgb);

      let current_cluster_timestamp = match self.current_cluster_start.as_ref() {
        None => 0i16,
        Some(start) => {
          let delta = frame.timestamp.duration_since(*start);
          (openh264::Timestamp::ZERO + delta)
            .as_millis()
            .try_into()
            .with_context(|| "overflow timestamp")?
        }
      };

      let segment_timestamp = self.segment_start.map_or(Ok(0u64), |start| {
        std::time::Instant::now()
          .duration_since(start)
          .as_millis()
          .try_into()
          .with_context(|| "segment timestamp overflow")
      })?;

      log::trace!("frame={segment_timestamp} cluster={current_cluster_timestamp}");

      let bitstream = self
        .encoder
        .encode_at(&yuv_buffer, openh264::Timestamp::from_millis(segment_timestamp))
        .with_context(|| "invalid input data; unable to perform openh264 encoding")?;

      log::trace!("we have an openh264 frame - {:?}", bitstream.frame_type());

      if self.segment_start.is_none() {
        log::info!("we have received our very first frame");
        self.segment_start = Some(frame.timestamp);
      }

      if self.current_cluster_start.is_none() {
        log::trace!("we have started a new cluster");
        self.current_cluster_start = Some(frame.timestamp);
      }

      self
        .mkv_cursor
        .add_segment(&mkv::SegmentFrame {
          bitstream,
          cluster_timestamp: current_cluster_timestamp,
          timestamp: frame.timestamp,
          dimensions: frame.dimensions,
        })
        .with_context(|| "unable to write bitstream segment into container")?;

      self.written_frames += 1;

      if self.written_frames > MAX_BUFFERED_FRAMES {
        self.current_cluster_start = None;
        log::debug!("flushing our current frames into the destination");
        self.mkv_cursor.dump_to(destination).await?;
        self.written_frames = 0;
      }

      Ok(())
    }
  }

  /// This method handles the initial HTTP handshake(s) with aws that are required to end up with a
  /// secure TCP connection that we can start dumping bytes into.
  async fn connect(
    credentials: &kvs::KVSCredentials,
  ) -> anyhow::Result<async_tls::client::TlsStream<async_std::net::TcpStream>> {
    let endpoint = kvs::fetch_data_endpoint(credentials).await?;
    log::info!("we have our kvs endpoint - '{endpoint}'");
    let signed_request = kvs::build_signed_request(credentials, &endpoint)?;

    // Now that we have our `http::Request`, we can actually start the creation of our http
    // connection.
    let socket_addr = signed_request
      .uri()
      .authority()
      .ok_or_else(|| anyhow::Error::msg("signed request has no host!"))?
      .clone();

    log::info!("establishing secure (tls) connection to '{}'", socket_addr.host());

    let connection = async_std::net::TcpStream::connect(format!("{}:443", socket_addr.host())).await?;
    let connector = async_tls::TlsConnector::default();
    let mut connection = connector
      .connect(socket_addr.host(), connection)
      .await
      .with_context(|| "unable to establish tls connection")?;

    let mut req = Vec::with_capacity(1024);
    req.extend_from_slice(b"POST /putMedia HTTP/1.1\r\n");

    let handshake_headers = [
      ("Host", socket_addr.host()),
      ("Connection", "keep-alive"),
      ("Accept", "*/*"),
      ("Accept-Encoding", "deflate, gzip"),
      ("Transfer-Encoding", "chunked"),
    ];

    for (k, v) in handshake_headers {
      let header = format!("{k}: {v}\r\n");
      req.extend_from_slice(header.as_bytes());
    }

    for header in signed_request.headers() {
      let key = header.0;
      let value = header.1.to_str().unwrap();
      let header = format!("{}: {value}\r\n", key.as_str());
      req.extend_from_slice(header.as_bytes());
    }

    req.extend_from_slice(b"\r\n");

    log::trace!(
      "writing http request for putmedia:\n{}",
      std::str::from_utf8(&req).unwrap()
    );

    let written = async_std::io::WriteExt::write(&mut connection, &req)
      .await
      .with_context(|| "unable to write our initial request to kvs")?;

    log::debug!("wrote {written} bytes over our kvs connection for initial handshake");

    let mut buf = [0u8; 1024];
    let received = async_std::io::ReadExt::read(&mut connection, &mut buf)
      .await
      .with_context(|| "error receiving response from kvs")?;

    log::debug!("received {received} bytes from kvs");

    let mut headers = [httparse::EMPTY_HEADER; 10];
    let mut response = httparse::Response::new(&mut headers);
    response.parse(&buf[0..received]).with_context(|| {
      let body = std::str::from_utf8(&buf[0..received]);
      format!("unable to parse response aws handshake response:\n{body:?}")
    })?;

    log::info!("parsed response successfully - '{:?}'", response.code);

    response
      .code
      .is_some_and(|c| c == 200)
      .then_some(connection)
      .ok_or_else(|| anyhow::Error::msg("invalid response code from aws handshake response"))
  }

  /// This type is being used internally by our `run` method to differentiate between the two tasks
  /// that we race while our loop is hot.
  enum NetworkFrame {
    /// We have read network from the opened TCP connection with kvs.
    ReceivedData(Vec<u8>),
    /// We have some command that was sent from the camera thread that we need to process.
    ReceivedCommand(StreamControl),
  }

  /// This function is the internals of the streaming thread. Here we attempt to open our connection
  /// with AWS, and perform the receiving, encoding, and transmission of image data up.
  pub async fn run(
    credentials: &kvs::KVSCredentials,
    camera_control: async_std::channel::Sender<CameraControl>,
    stream_control: async_std::channel::Receiver<StreamControl>,
  ) -> anyhow::Result<()> {
    let mut encoder = NetworkedVideoEncoder::new()?;
    let mut connection = connect(credentials).await?;

    camera_control
      .send(CameraControl::NetworkReady)
      .await
      .with_context(|| "unable to notify camera of network connection readiness")?;

    log::info!("network connection established, starting data streaming");

    loop {
      let control = async {
        stream_control
          .recv()
          .await
          .with_context(|| "stream control channel shutdown")
          .map(NetworkFrame::ReceivedCommand)
      };

      let read = async {
        let mut buf = [0u8; 1024];
        let size = async_std::io::ReadExt::read(&mut connection, &mut buf)
          .await
          .with_context(|| "unable to read from connection, closing")?;

        if size == 0 {
          return Err(anyhow::Error::msg("aws connection appears closed - read 0 bytes"));
        }

        let payload = &buf[0..size];
        Ok(NetworkFrame::ReceivedData(payload.to_vec()))
      };

      match futures_lite::future::race(control, read).await {
        Err(error) => {
          log::error!("internal error during network stream loop - '{error}'");
          break;
        }
        Ok(NetworkFrame::ReceivedCommand(StreamControl::Terminate)) => {
          log::info!("network thread received graceful notification of termination from camera");
          return Ok(());
        }
        Ok(NetworkFrame::ReceivedCommand(StreamControl::DataAvailable(frame))) => {
          log::trace!(
            "received an image from the camera thread with dimension = {:?}",
            frame.dimensions
          );
          // TODO(optimization) - here we are letting our encoder fill a buffer for us, which we
          // then manually write into the open tcp connection using a chunked transfer encoding.
          let mut encoded_buffer =
            Vec::with_capacity(frame.dimensions.0 as usize * frame.dimensions.1 as usize);

          encoder.encode_to(&frame, &mut encoded_buffer).await?;

          if !encoded_buffer.is_empty() {
            log::debug!("encoder readied data, writing to connection now");

            // see: <https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Transfer-Encoding>
            let mut copied = Vec::with_capacity(encoded_buffer.len());
            let hex_size = format!("{:X}", encoded_buffer.len());
            copied.extend_from_slice(hex_size.as_bytes());
            copied.extend_from_slice(b"\r\n");
            copied.extend_from_slice(&encoded_buffer);
            copied.extend_from_slice(b"\r\n");

            async_std::io::WriteExt::write_all(&mut connection, &copied)
              .await
              .with_context(|| "unable to write chunked stream data to kvs connection")?;
          }
        }
        Ok(NetworkFrame::ReceivedData(data)) => {
          let payload = String::from_utf8(data)
            .with_context(|| "invalid, non-utf8 data recieved from kvs")
            .and_then(|chunked| {
              let mut lines = chunked.split("\r\n");
              let (size, payload) = (lines.next(), lines.next());

              // TODO: eventually this _should_ be used to validate/truncate the data we are
              // parsing so that we avoid over extending `serde_json`.
              let _ = size.map_or_else(
                || Err(anyhow::Error::msg("aws response was not chunked")),
                |value| {
                  value
                    .parse::<usize>()
                    .with_context(|| "aws response chunk did not include a valid size")
                },
              )?;

              let payload = payload.map_or_else(
                || Err(anyhow::Error::msg("missing payload in aws chunk response")),
                |payload| {
                  serde_json::from_str::<kvs::KVSResponse>(payload)
                    .with_context(|| "unable to parse chunked kvs response as json")
                },
              )?;

              Ok(payload)
            })?;

          log::info!("received valid JSON response from kvs - '{payload:?}'");
        }
      };
    }

    log::info!("network thread termination, sending camera termination");
    camera_control
      .send(CameraControl::Terminate)
      .await
      .with_context(|| "unable to notify video of stream termination")?;

    Ok(())
  }
}

/// The camera module defines.
pub mod camera {
  use super::{CameraControl, StreamControl};
  use crate::{camera, monitoring};
  use anyhow::Context;

  /// This type is used by the camera thread to track what our framerate is, for development
  /// purposes.
  struct FramerateStats {
    /// The last time we logged.
    last: std::time::Instant,
    /// The amount of frames we have seen.
    count: usize,
  }

  impl Default for FramerateStats {
    fn default() -> Self {
      Self {
        last: std::time::Instant::now(),
        count: 0,
      }
    }
  }

  impl FramerateStats {
    /// Increments our frames, potentially logging out how many we have seen since the last log
    /// if we exceed 1s.
    fn tick(&mut self) {
      let now = std::time::Instant::now();

      if now.duration_since(self.last).as_millis() > 1000 {
        log::debug!("{} fps", self.count);
        self.last = now;
        self.count = 1;
        return;
      }

      self.count += 1;
    }
  }

  /// This method is responsible for entering the main runloop for our camera; here we will wait
  /// for acknowledgement from the network that we are connected, and simply try to read image data
  /// from the videoio device as frequently as possible.
  pub async fn run<P>(
    mut device: camera::Camera<P>,
    stream_control: async_std::channel::Sender<StreamControl>,
    camera_control: async_std::channel::Receiver<CameraControl>,
  ) -> anyhow::Result<()>
  where
    P: AsRef<str> + std::fmt::Debug,
  {
    log::info!("entering main camera run loop");

    loop {
      let update = camera_control
        .recv()
        .await
        .with_context(|| "unable to receive initial readiness from network channel")?;

      if matches!(update, CameraControl::NetworkReady) {
        log::info!("our camera is ready, starting main image loop");
        break;
      }
    }

    let mut stats = FramerateStats::default();
    let mut monitor = monitoring::Stats::default().rename("camera-stream");

    loop {
      stats.tick();

      let should_exit = monitor.increment("camera-recv", || match camera_control.try_recv() {
        Err(async_std::channel::TryRecvError::Empty) => false,
        Err(async_std::channel::TryRecvError::Closed) | Ok(CameraControl::NetworkReady) => true,
        Ok(control) => {
          log::info!("camera thread received control message - '{control:?}'");
          false
        }
      });

      if should_exit {
        break;
      }

      let jpeg = monitor.increment("take-jpeg", || {
        device
          .take_jpeg_frame()
          .with_context(|| "unable to pull image data from camera")
      })?;

      let end_send = monitor.take("send-jpeg");
      stream_control
        .send(StreamControl::DataAvailable(jpeg))
        .await
        .with_context(|| "unable to send data to network thread")?;
      end_send();
    }

    stream_control
      .send(StreamControl::Terminate)
      .await
      .with_context(|| "unable to notify stream of video termination")?;
    Ok(())
  }
}
