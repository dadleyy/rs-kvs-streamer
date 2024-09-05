#![deny(clippy::pedantic, clippy::missing_docs_in_private_items, missing_docs)]

//! This application demonstrates how one might building an application that streams image data
//! read from an opencv device into the amazon web service's kinesis video streams.

use anyhow::Context;
use clap::Parser;
use std::io;

/// TODO.
mod avcc;
/// TODO.
mod camera;
/// TODO.
mod kvs;
/// TODO.
mod mkv;
/// TODO.
mod monitoring;
/// TODO.
mod streaming;

/// Our command line "schema"; this is parsed by clap, by way of the derived `Parser` trait here.
#[derive(Parser)]
#[clap(version = option_env!("RS_KVS_STREAMER_VERSION").unwrap_or("dev"))]
struct CommandLine {
  /// The actual command we want to run. There may be additional fields we want to expose in the
  /// command line, but until it is clear what would truly be shared between commands, they are
  /// pushed down into the subcommands.
  #[clap(subcommand)]
  subcommand: Subcommand,
}

/// The schema of each subcommand.
#[derive(Clone, Debug, clap::Subcommand)]
enum Subcommand {
  /// This subcommand will attempt to actually run the main streaming logic.
  Stream {
    /// Which video device to use. E.g - `/dev/video0`.
    #[clap(short, long)]
    device: String,
  },

  /// For capturing, we want the user to provide a video device and the location where we will
  /// save the file.
  Capture {
    /// Which video device to use. E.g - `/dev/video0`.
    #[clap(short, long)]
    device: String,
    /// Where to write the jpeg data.
    #[clap(short, long)]
    file: String,
    /// Whether or not to overwrite any existing files.
    #[clap(default_value = "false", short, long)]
    overwrite: bool,
  },
}

/// The purpose of this function is to move us from `io::Result` land into `anyhow::Result` land;
/// this is handy for the majority of our code here, which will have to deal with various error
/// types.
#[allow(clippy::too_many_lines)]
fn run() -> anyhow::Result<()> {
  let cli = CommandLine::parse();

  match cli.subcommand {
    Subcommand::Stream { device } => {
      log::info!("loading credentials from the environment");
      let creds =
        kvs::KVSCredentials::from_env().with_context(|| "unable to load aws credentials from env")?;

      log::info!("attempting to open camera '{device}'");
      let camera = camera::Camera::open(camera::DeviceConnectionType::ByPath(device))?;

      let (stream_killer, stream_killing) = async_std::channel::bounded(1);
      let (camera_killer, camera_killing) = async_std::channel::bounded(1);

      // This channel is used by our main thread to receive notifications when any of our other
      // threads have completed. Once we have received notifications that all threads have been
      // terminated, we will join them.
      let (thread_completions, thread_receiver) = async_std::channel::unbounded();

      log::info!("spawing our kvs streaming thread");
      let stream_complete = thread_completions.clone();
      let streaming = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
          .enable_all()
          .build()?;

        log::debug!("streaming async runtime ready, entering runloop");

        let sc = camera_killer.clone();

        let out = runtime
          .block_on(async move { streaming::network::run(&creds, camera_killer, stream_killing).await });

        log::debug!("network thread terminated, coorindating thread shutdown");

        if let Err(ref e) = out {
          log::warn!("network thread exited with an error - '{e:?}'");

          if let Err(ref e) = sc.send_blocking(streaming::CameraControl::Terminate) {
            log::warn!("network thread unable to notify termination to camera thread - '{e}'");
          }
        }

        if let Err(error) = stream_complete.send_blocking(()) {
          log::error!("unable to notify stream thread is complete - '{error}'");
        }

        out
      });

      log::info!("spawing our main video input thread");
      let camera_complete = thread_completions.clone();
      let watching = std::thread::spawn(move || {
        log::info!("video thread spawned");

        let local_ex = async_executor::LocalExecutor::new();

        let sc = stream_killer.clone();

        let out = futures_lite::future::block_on(
          local_ex.run(async move { streaming::camera::run(camera, stream_killer, camera_killing).await }),
        );

        log::debug!("camera thread terminated, coordinating thread shutdown");

        if let Err(ref e) = out {
          log::warn!("camera thread terminated with error - '{e}'");

          if let Err(ref e) = sc.send_blocking(streaming::StreamControl::Terminate) {
            log::warn!("unable to notify network thread of camera shutdown - '{e}'");
          }
        }

        if let Err(error) = camera_complete.send_blocking(()) {
          log::error!("unable to notify camera thread completion - {error:?}");
        }

        out
      });

      log::info!("streaming + camera tbhreads spawned waiting for completions");

      // Now we spawn an async runtime that will block this main thread until we have been notified
      // that both of the two threads - camera input and network - have completed.
      let local_ex = async_executor::LocalExecutor::new();
      let thread_result = futures_lite::future::block_on(local_ex.run(async {
        log::debug!("blocking main thread on streaming + video threads");
        let mut completions = 0;

        while completions < 2 {
          thread_receiver
            .recv()
            .await
            .with_context(|| "thread coordination channel unexpectedly closed")?;
          completions += 1;
        }

        log::info!("all threads successfully terminated");
        Ok::<(), anyhow::Error>(())
      }));

      if let Err(error) = thread_result {
        log::error!("thread management unexpectedly failed - {error:?}");
      }

      match watching.join() {
        Err(error) => log::error!("fatal error on video thread - {error:?}"),
        Ok(Err(error)) => log::warn!("video thread exited with error - '{error}'"),
        Ok(_) => log::debug!("video thread terminated without error"),
      }

      match streaming.join() {
        Err(error) => log::error!("fatal error on streaming thread - {error:?}"),
        Ok(Err(error)) => log::warn!("streaming thread exited with error - '{error}'"),
        Ok(_) => log::debug!("streaming thread terminated without error"),
      }
    }
    Subcommand::Capture {
      device,
      file,
      overwrite,
    } => {
      log::info!("attemping to open '{device}' and save an image to '{file}'");

      match (overwrite, std::fs::metadata(&file)) {
        (true, Ok(_)) => {
          log::info!("'{file}' already exists but we can delete");
          std::fs::remove_file(&file).with_context(|| format!("unable to delete '{file}'"))?;
          log::debug!("'{file}' successfully deleted");
        }
        (false, Ok(_)) => {
          log::info!("file already exists and we cant delete");
          let err = format!("'{file}' appears to exist. Capturing can automatically overwrite the specified file via the --overwrite flag");
          return Err(anyhow::Error::msg(err));
        }
        (_, Err(_)) => {
          log::info!("file does not exist");

          std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&file)
            .with_context(|| format!("could not open '{file}'"))?;
        }
      }

      let mut cam = camera::Camera::open(camera::DeviceConnectionType::ByPath(&device))?;

      let mut file_handle = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&file)
        .with_context(|| format!("could not open '{file}'"))?;

      let mut reader = std::io::Cursor::new(cam.take_jpeg()?);
      std::io::copy(&mut reader, &mut file_handle).with_context(|| format!("unable to write to '{file}'"))?;
    }
  }

  Ok(())
}

#[allow(clippy::unnecessary_wraps)]
fn main() -> io::Result<()> {
  let _ = dotenv::dotenv();

  env_logger::Builder::from_default_env()
    .format_timestamp(Some(env_logger::TimestampPrecision::Millis))
    .init();

  run().map_err(|error| io::Error::new(io::ErrorKind::Other, format!("{error:?}")))
}
