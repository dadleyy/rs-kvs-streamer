//! This module provides a thin abstraction on top of the `opencv::videoio` module, typically
//! batching a few method calls into a single api.

use anyhow::Context;

/// This type holds the subset of information that we care about providing between our camera/video
/// thread and the network thread.
pub struct JpegFrame {
  /// When this frame was captured.
  pub timestamp: std::time::Instant,
  /// The jpeg data pulled from the video device.
  pub data: Vec<u8>,
  /// The width and height of this frame, per our last read properties.
  pub dimensions: (u16, u16),
}

impl std::fmt::Debug for JpegFrame {
  fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
    let (width, height) = self.dimensions;
    write!(formatter, "({width} x {height}) at {:?}", self.timestamp)
  }
}

impl JpegFrame {
  /// Returns the size, in bytes, of our jpeg data.
  #[allow(dead_code)]
  pub fn len(&self) -> usize {
    self.data.len()
  }

  /// Returns true when the buffer we are holding is empty.
  #[allow(dead_code)]
  pub fn is_empty(&self) -> bool {
    self.data.is_empty()
  }
}

/// Eventually, we will want to support many, cross platform ways for users to provide the video
/// device they want to connect to. This type represents the start of that effort.
#[derive(Debug)]
pub enum DeviceConnectionType<P>
where
  P: AsRef<str> + std::fmt::Debug,
{
  /// Will attempt to use the `opencv::videoio::VideoCapture::from_file` api.
  ByPath(P),
}

/// This type is used to wrap up the `opencv::videoio::CAP_*` related constants, providing us a
/// "newtype" that we can use to add formatting and other apis to.
#[derive(Debug, Hash, PartialEq, Eq, Clone, Copy, PartialOrd, Ord)]
pub enum Property {
  /// `opencv::videoio::CAP_PROP_FOURCC`
  FourCC,
  /// `opencv::videoio::CAP_PROP_FORMAT`
  Format,
  /// `opencv::videoio::CAP_PROP_FPS`,
  Fps,
  /// `opencv::videoio::CAP_PROP_CONVERT_RGB`
  ConvertRGB,
  /// `opencv::videoio::CAP_PROP_BUFFERSIZE`
  BufferSize,
  /// `opencv::videoio::CAP_PROP_FRAME_WIDTH`
  Width,
  /// `opencv::videoio::CAP_PROP_FRAME_HEIGHT`
  Height,
  /// Unknown
  Unknown(i32),
}

impl From<Property> for i32 {
  fn from(val: Property) -> i32 {
    match val {
      Property::FourCC => opencv::videoio::CAP_PROP_FOURCC,
      Property::Fps => opencv::videoio::CAP_PROP_FPS,
      Property::Width => opencv::videoio::CAP_PROP_FRAME_WIDTH,
      Property::Height => opencv::videoio::CAP_PROP_FRAME_HEIGHT,
      Property::Format => opencv::videoio::CAP_PROP_FORMAT,
      Property::BufferSize => opencv::videoio::CAP_PROP_BUFFERSIZE,
      Property::ConvertRGB => opencv::videoio::CAP_PROP_CONVERT_RGB,
      Property::Unknown(id) => id,
    }
  }
}

impl From<i32> for Property {
  fn from(input: i32) -> Self {
    match input {
      opencv::videoio::CAP_PROP_FOURCC => Self::FourCC,
      opencv::videoio::CAP_PROP_FORMAT => Self::Format,
      opencv::videoio::CAP_PROP_FPS => Self::Fps,
      opencv::videoio::CAP_PROP_FRAME_WIDTH => Self::Width,
      opencv::videoio::CAP_PROP_FRAME_HEIGHT => Self::Height,
      opencv::videoio::CAP_PROP_CONVERT_RGB => Self::ConvertRGB,
      opencv::videoio::CAP_PROP_BUFFERSIZE => Self::BufferSize,
      other => Self::Unknown(other),
    }
  }
}

/// The main abstraction over the `opencv::videoio::VideoCapture` instance.
pub struct Camera<P>
where
  P: AsRef<str> + std::fmt::Debug,
{
  /// A useful utility for monitoring where time is most spent.
  stats: crate::monitoring::Stats,
  /// The underlying opencv video device we will be interacting with.
  device: opencv::videoio::VideoCapture,
  /// For the time being, we want to keep our original connection information around (in the event
  /// of the desire to provide the ability to reconnect). We also want our connection to be
  /// somewhat generic over the types of "paths" we can accept, so we need to carry around the
  /// generics and trait bounds here.
  connection: DeviceConnectionType<P>,
  /// The current properties we are aware of.
  properties: std::collections::BTreeMap<Property, f64>,
}

/// At various times, we will want to populate our properties map with the set of fields + values
/// that we are actually interested with here, which is currently a subset of those exposed through
/// opencv.
fn fill_properties(
  device: &mut opencv::videoio::VideoCapture,
  properties: &mut std::collections::BTreeMap<Property, f64>,
) -> anyhow::Result<()> {
  for property in [
    Property::FourCC,
    Property::Width,
    Property::Height,
    Property::BufferSize,
    Property::ConvertRGB,
    Property::Fps,
    Property::Format,
  ] {
    let value = opencv::videoio::VideoCaptureTraitConst::get(device, property.into())
      .with_context(|| format!("unable to read {property:?}"))?;

    properties.insert(property, value);
  }

  log::debug!("properties ready - {properties:?}");

  Ok(())
}

impl<P> Camera<P>
where
  P: AsRef<str> + std::fmt::Debug,
{
  /// This method provides the abstraction of dealing with the potential for cross-platform video
  /// support api differences, with the addition of some initial validation that we do indeed
  /// have a video device.
  pub fn open(connection: DeviceConnectionType<P>) -> anyhow::Result<Self> {
    let mut device = match &connection {
      DeviceConnectionType::ByPath(p) => {
        opencv::videoio::VideoCapture::from_file(p.as_ref(), opencv::videoio::CAP_V4L)
          .with_context(|| format!("unable to open device by path '{}'", p.as_ref()))?
      }
    };

    log::info!("verifying that '{connection:?}' is actually available");

    opencv::videoio::VideoCaptureTraitConst::is_opened(&device)
      .with_context(|| format!("unable to verify '{connection:?}'"))?
      .then_some(())
      .ok_or_else(|| {
        anyhow::Error::msg(format!(
          "unable to verify access to video device '{connection:?}'"
        ))
      })?;

    let mut properties = std::collections::BTreeMap::new();
    fill_properties(&mut device, &mut properties)
      .with_context(|| format!("property failure for '{connection:?}'"))?;

    log::info!("'{connection:?}' video device is ready - {properties:?}");

    Ok(Self {
      stats: crate::monitoring::Stats::default().rename("camera"),
      device,
      connection,
      properties,
    })
  }

  /// Internally calls `take_jpeg`, filling in the timestamp and dimensions. The dimensions are
  /// pulled from whatever we have previously stored in our property map (instead of from the video
  /// device itself).
  pub fn take_jpeg_frame(&mut self) -> anyhow::Result<JpegFrame> {
    let data = self.take_jpeg()?;
    let timestamp = std::time::Instant::now();
    let f_width = self
      .properties
      .get(&Property::Width)
      .ok_or_else(|| anyhow::Error::msg("the video device did not appear to have a width property"))?;
    let f_height = self
      .properties
      .get(&Property::Height)
      .ok_or_else(|| anyhow::Error::msg("the video device did not appear to have a height property"))?;

    #[allow(clippy::cast_lossless)]
    if *f_width > (u16::MAX as u32) as f64 || *f_width < 0.0 {
      return Err(anyhow::Error::msg(
        "the width property of the device is out of bounds",
      ));
    }

    #[allow(clippy::cast_lossless)]
    if *f_height > (u16::MAX as u32) as f64 || *f_height < 0.0 {
      return Err(anyhow::Error::msg(
        "the height property of the device is out of bounds",
      ));
    }

    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let height = (*f_height as u64)
      .try_into()
      .with_context(|| "invalid height value")?;

    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let width = (*f_width as u64)
      .try_into()
      .with_context(|| "invalid width value")?;

    Ok(JpegFrame {
      data,
      timestamp,
      dimensions: (width, height),
    })
  }

  /// This method abstracts away the api contract for configuring our camera to produce jpeg image
  /// data explicitly.
  pub fn take_jpeg(&mut self) -> anyhow::Result<Vec<u8>> {
    let fourcc = opencv::videoio::VideoWriter::fourcc('M', 'J', 'P', 'G')
      .with_context(|| "opencv api erorr when constructing fourcc".to_string())?;

    let current_fourcc = self.stats.increment("prop-lookup", || {
      self
        .properties
        .get(&Property::FourCC)
        .copied()
        .unwrap_or_default()
    });

    #[allow(clippy::float_cmp, clippy::cast_lossless)]
    if current_fourcc != fourcc as f64 {
      log::warn!("device does not appear ready for jpeg captures, configuring now");

      self.stats.increment("property-set:rgb", || {
        opencv::videoio::VideoCaptureTrait::set(&mut self.device, opencv::videoio::CAP_PROP_CONVERT_RGB, 0.0)
          .with_context(|| {
            format!(
              "unable to prepare '{:?}' opencv settings for capture",
              self.connection
            )
          })
      })?;

      self.stats.increment("property-set:buffer", || {
        opencv::videoio::VideoCaptureTrait::set(&mut self.device, opencv::videoio::CAP_PROP_BUFFERSIZE, 1.0)
          .with_context(|| {
            format!(
              "unable to prepare '{:?}' opencv settings for capture",
              self.connection
            )
          })
      })?;

      self.stats.increment("property-set:fourcc", || {
        opencv::videoio::VideoCaptureTrait::set(
          &mut self.device,
          opencv::videoio::CAP_PROP_FOURCC,
          fourcc.into(),
        )
        .with_context(|| {
          format!(
            "unable to prepare '{:?}' opencv settings for capture",
            self.connection
          )
        })
      })?;

      self.stats.increment("fill-properties", || {
        fill_properties(&mut self.device, &mut self.properties)
          .with_context(|| format!("property failure for '{:?}'", self.connection))
      })?;
    }

    self.stats.increment("opencv::grab", || {
      opencv::videoio::VideoCaptureTrait::grab(&mut self.device)
        .is_ok_and(|inner| inner)
        .then_some(())
        .ok_or_else(|| anyhow::Error::msg(format!("unable to ready image data from '{:?}'", self.connection)))
    })?;

    let mut buffer = opencv::core::Mat::default();

    self.stats.increment("opencv::retrieve", || {
      opencv::prelude::VideoCaptureTrait::retrieve(&mut self.device, &mut buffer, 0)
        .with_context(|| format!("unable to pull buffer from '{:?}'", self.connection))?
        .then_some(())
        .ok_or_else(|| anyhow::Error::msg(format!("failed retrieving data from '{:?}'", self.connection)))
    })?;

    let size = opencv::core::MatTraitConst::rows(&buffer) * opencv::core::MatTraitConst::cols(&buffer);
    assert!(size > 0);

    #[allow(clippy::cast_sign_loss)]
    let mut vector: opencv::core::Vector<u8> = self.stats.increment("allocate-vector", || {
      opencv::core::Vector::with_capacity(size as usize)
    });

    self.stats.increment("opencv::copy_to", || {
      opencv::core::MatTraitConst::copy_to(&buffer, &mut vector)
        .with_context(|| "unable to copy image data into vector")
    })?;

    Ok(
      self
        .stats
        .increment("vector-collect", move || vector.into_iter().collect()),
    )
  }
}
