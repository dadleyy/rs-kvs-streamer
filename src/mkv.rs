use super::avcc;
use anyhow::Context;
use h264_reader::nal::Nal;
use std::io::BufRead;
use webm_iterable::matroska_spec;

/// This type is meant to associate the encoded frame of openh264 data with a timestamp.
pub struct SegmentFrame<'a> {
  /// The encoded h264 data.
  pub(crate) bitstream: openh264::encoder::EncodedBitStream<'a>,
  /// The timestamp (relative to the start?). This comes from the Matroska specification:
  ///
  /// > "The Block Element and SimpleBlock Element store their timestamps as 16bit signed
  /// > integers..."
  ///
  /// <https://www.matroska.org/technical/notes.html>
  pub(crate) cluster_timestamp: i16,
  /// The global timestamp
  pub(crate) timestamp: std::time::Instant,
  /// The height and width.
  pub(crate) dimensions: (u16, u16),
}

impl<'a> SegmentFrame<'a> {
  /// This method is responsible for constructing the appropriate MKV tag/container elements
  /// based on the openh264 (+ other) contents we are carrying.
  fn simpleblock(&self) -> anyhow::Result<matroska_spec::MatroskaSpec> {
    let mut bitstream_data = Vec::with_capacity(10_000);

    self
      .bitstream
      .write(&mut bitstream_data)
      .with_context(|| "unable to read bitstream")?;

    let mut parsed_nal_data = Vec::with_capacity(10_000);

    let mut reader = h264_reader::annexb::AnnexBReader::accumulate(|h: h264_reader::nal::RefNal<'_>| {
      if !h.is_complete() || h.header().is_err() {
        return h264_reader::push::NalInterest::Buffer;
      }

      let mut byte_reader = h.reader();

      let Ok(content) = byte_reader.fill_buf() else {
        return h264_reader::push::NalInterest::Buffer;
      };

      let Ok(unit_len) = u32::try_from(content.len()) else {
        return h264_reader::push::NalInterest::Buffer;
      };

      parsed_nal_data.extend_from_slice(&unit_len.to_be_bytes());
      parsed_nal_data.extend_from_slice(content);

      h264_reader::push::NalInterest::Ignore
    });

    reader.push(bitstream_data.as_slice());
    reader.reset();

    let mut block_buffer = Vec::with_capacity(30);
    block_buffer.extend_from_slice(&1u64.to_le_bytes());
    block_buffer.extend_from_slice(&1u64.to_le_bytes());
    block_buffer.extend_from_slice(&0u64.to_le_bytes());

    let frame = matroska_spec::Frame {
      data: &parsed_nal_data,
    };

    let block_tag = matroska_spec::MatroskaSpec::SimpleBlock(block_buffer);

    let mut block: matroska_spec::SimpleBlock = (&block_tag)
      .try_into()
      .with_context(|| "simple block allocation failed")?;

    block.lacing = None;
    block.track = 1;
    block.keyframe = false;
    block.timestamp = self.cluster_timestamp;

    log::trace!("created simple block @ timestamp = {}", block.timestamp);

    block.set_frame_data(&vec![frame]);

    Ok(block.into())
  }
}

/// A wrapper around the `WebmWriter` that automatically frees memory from tags that have been
/// completely written.
pub(super) struct Cursor {
  /// The inner [`webm_iterable::WebmWriter`] interface over the in-memory vector we will be
  /// managing.
  writer: webm_iterable::WebmWriter<Vec<u8>>,
  /// We're keeping track of the position in our buffer where the `Tracks` element was written.
  /// Everything after this can be safely discarded every time we flush our current buffer over the
  /// network.
  tracks_end: Option<usize>,
  /// This field stores the moment we started the entire segment (which is made of many clusters).
  /// Once we have received a frame, this is set, and subsequent segments will use the duration
  /// since this as their relative timestamp.
  segment_start: Option<std::time::Instant>,
  /// This field stores the number of frames that we have written to the currently open cluster
  /// since our last flush.
  frame_index: usize,
  /// The total number of frames seen.
  frames_seen: usize,
  /// TODO
  position: usize,
  /// TODO
  codec_private: Option<Vec<u8>>,
}

impl Default for Cursor {
  fn default() -> Self {
    Cursor::new(Vec::with_capacity(10_000))
  }
}

impl Cursor {
  /// Constructs our "cursor" from some buffer that has been provided to us.
  pub(super) fn new(buffer: Vec<u8>) -> Self {
    Self {
      writer: webm_iterable::WebmWriter::new(buffer),
      segment_start: None,
      frame_index: 0,
      frames_seen: 0,
      tracks_end: None,
      position: 0,
      codec_private: None,
    }
  }

  /// This method is implemented as a convenience for mapping the `Result` type of the underlying
  /// mkv writer into the `Result` type we are consistent with here.
  fn write_tag(&mut self, tag: &matroska_spec::MatroskaSpec) -> anyhow::Result<()> {
    self
      .writer
      .write(tag)
      .with_context(|| "unable to append our tag to the internal embl writer")
  }

  /// This method is responsible for writing into our internal mkv container the necessary tags
  /// that come before any of our actual video data.
  fn write_tracks(&mut self, block: &SegmentFrame) -> anyhow::Result<()> {
    self.codec_private = match avcc::generate_private_data(&block.bitstream) {
      Ok(data) => Some(data),
      Err(error) => {
        log::error!("unable to populate private data - {error}");
        None
      }
    };

    [
      matroska_spec::MatroskaSpec::Tracks(matroska_spec::Master::Start),
      matroska_spec::MatroskaSpec::TrackEntry(matroska_spec::Master::Start),
      matroska_spec::MatroskaSpec::TrackNumber(1),
      matroska_spec::MatroskaSpec::TrackType(1),
      matroska_spec::MatroskaSpec::TrackTimestampScale(1.0),
      matroska_spec::MatroskaSpec::TrackUID(0x55),
      matroska_spec::MatroskaSpec::Name(uuid::Uuid::new_v4().to_string()),
      matroska_spec::MatroskaSpec::CodecID("V_MPEG4/ISO/AVC".to_string()),
      matroska_spec::MatroskaSpec::CodecName("AVC/H.264".to_string()),
    ]
    .into_iter()
    .try_for_each(|tag| self.write_tag(&tag))?;

    log::info!("writing initial track + track entry tags to mkv buffer");

    if let Some(ref private) = self.codec_private {
      log::debug!("codec private data ({} bytes) {private:X?}", private.len());
      self.write_tag(&matroska_spec::MatroskaSpec::CodecPrivate(private.clone()))?;
    }

    [
      matroska_spec::MatroskaSpec::Video(matroska_spec::Master::Start),
      matroska_spec::MatroskaSpec::PixelWidth(block.dimensions.0.into()),
      matroska_spec::MatroskaSpec::PixelHeight(block.dimensions.1.into()),
      matroska_spec::MatroskaSpec::FlagInterlaced(2),
      matroska_spec::MatroskaSpec::DisplayUnit(4),
      matroska_spec::MatroskaSpec::DisplayWidth(block.dimensions.0.into()),
      matroska_spec::MatroskaSpec::DisplayHeight(block.dimensions.1.into()),
      matroska_spec::MatroskaSpec::Video(matroska_spec::Master::End),
      matroska_spec::MatroskaSpec::TrackEntry(matroska_spec::Master::End),
      matroska_spec::MatroskaSpec::Tracks(matroska_spec::Master::End),
    ]
    .into_iter()
    .try_for_each(|tag| self.write_tag(&tag))?;

    self.tracks_end = Some(self.writer.get_ref().len());

    Ok(())
  }

  /// This method is responsible for adding the timestamped video data into our internal mkv
  /// writer. Along the way, we check to see if do not currently have a segment start, and create
  /// any segment tags that are neaded.
  pub(super) fn add_segment(&mut self, block: &SegmentFrame) -> anyhow::Result<()> {
    // Determine the timestamp that we would use
    let cluster_timestamp = self.segment_start.map_or(Ok(0u64), |start| {
      block.timestamp.duration_since(start).as_millis().try_into()
    })?;

    self.frames_seen += 1;

    // For our _very_ first write, we'll also want to create the opening trakcs tags.
    if self.segment_start.is_none() {
      self.segment_start = Some(block.timestamp);
      self.write_tracks(block)?;
    }

    // Whenever we dump our current cluster, we need to re-open the tag and determine the relative
    // starting tag that will be used.
    if self.frame_index == 0 {
      let tags = [
        matroska_spec::MatroskaSpec::Cluster(matroska_spec::Master::Start),
        matroska_spec::MatroskaSpec::Timestamp(cluster_timestamp),
      ];

      log::trace!("opening cluster tag (timestamp = {cluster_timestamp})",);

      for t in tags {
        self.write_tag(&t)?;
      }
    }

    log::trace!("#{}", self.frames_seen);
    self.write_tag(&block.simpleblock()?)?;

    self.frame_index += 1;

    Ok(())
  }

  /// This method is responsible for preparing the initial set of tags inside our mkv container.
  /// Once called, the `writer` held internally should be ready to start adding `Cluster` elements,
  /// which contain the actual video/audio data that we care about.
  pub(super) fn start(&mut self) -> anyhow::Result<()> {
    let tags = [
      matroska_spec::MatroskaSpec::Ebml(matroska_spec::Master::Start),
      matroska_spec::MatroskaSpec::DocType("matroska".to_owned()),
      matroska_spec::MatroskaSpec::DocTypeVersion(4),
      matroska_spec::MatroskaSpec::Ebml(matroska_spec::Master::End),
    ];

    for tag in &tags {
      self.write_tag(tag)?;
    }

    // The `Segment` tag is special; we are using an "unknown sized" element here. This is
    // important for streaming purposes.
    //
    // <https://www.matroska.org/technical/streaming.html>
    self
      .writer
      .write_advanced(
        &matroska_spec::MatroskaSpec::Segment(matroska_spec::Master::Start),
        webm_iterable::WriteOptions::is_unknown_sized_element(),
      )
      .with_context(|| "unable to open segment tag")?;

    let segment_uid = uuid::Uuid::new_v4();

    let info = [
      // ---- info start
      matroska_spec::MatroskaSpec::Info(matroska_spec::Master::Start),
      matroska_spec::MatroskaSpec::SegmentUID(segment_uid.as_bytes().to_vec()),
      matroska_spec::MatroskaSpec::Title(segment_uid.to_string()),
      matroska_spec::MatroskaSpec::TimestampScale(1_000_000),
      matroska_spec::MatroskaSpec::WritingApp("rs-kvs-streamer".to_string()),
      matroska_spec::MatroskaSpec::MuxingApp("rs-kvs-streamer".to_string()),
      matroska_spec::MatroskaSpec::Info(matroska_spec::Master::End),
    ];

    for tag in &info {
      self.write_tag(tag)?;
    }

    Ok(())
  }

  /// This function is important for managing the memory of our underlying `webm_iterable` writer;
  /// as we are accumulating tags inside its private buffer, it will write complete tags into the
  /// buffer we intiailized it with.
  pub async fn dump_to<W>(&mut self, mut destination: W) -> anyhow::Result<()>
  where
    W: async_std::io::Write + std::marker::Unpin,
  {
    log::trace!("closing cluster on frame={}", self.frame_index);

    let Some(track_end) = self.tracks_end else {
      return Ok(());
    };

    self.write_tag(&matroska_spec::MatroskaSpec::Cluster(matroska_spec::Master::End))?;
    self.frame_index = 0;

    let buffer = self.writer.get_ref();
    let subslice = buffer.as_slice()[self.position..].to_vec();
    let start: usize = self.position;

    if subslice.is_empty() {
      log::warn!("nothing new to write @ {start}");

      return Ok(());
    }

    self.position += subslice.len();

    // This logic exists to help keep our overall memory useage down. We are effectively truncating
    // the buffer used by the `webm_iterable` crate down to the position of the closing `Track` tag
    // every time we end up writing buffers to the `destination`.
    let before = self.writer.get_ref().len();
    let drained = self.writer.get_mut();
    drained.drain(track_end..);
    self.position = track_end;
    let after = self.writer.get_ref().len();

    log::debug!("truncated mkv buffer from {before} to {after} @ track end {track_end}");

    async_std::io::WriteExt::write_all(&mut destination, &subslice)
      .await
      .with_context(|| "failed writing current mkv buffer to destination")
  }
}
