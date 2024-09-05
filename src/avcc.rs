#![allow(clippy::similar_names)]

use anyhow::Context;
use h264_reader::nal::Nal;
use std::io;

/// Communicating with h264 encoded video data inside a matroska media container requires the
/// producer to send along a piece of "codec private" data early in the stream, which is used by
/// the consumer to decode the streamed data.
///
/// This method is responsible for construcing that private data. This is very much a work in
/// progress; the exact specification is tricky to understand, and the implementation within this
/// method may not be correct/optimal.
///
/// <https://github.com/aizvorski/h264bitstream/blob/ae72f73/h264_avcc.c#L70>
pub(crate) fn generate_private_data(
  bitstream: &openh264::encoder::EncodedBitStream,
) -> anyhow::Result<Vec<u8>> {
  let mut private_data = vec![];

  let mut written_data = Vec::default();
  bitstream
    .write(&mut written_data)
    .with_context(|| "unable to extract encoded h264 data")?;

  let mut found_sps: Option<(h264_reader::nal::sps::SeqParameterSet, Vec<u8>)> = None;
  let mut found_pps: Option<(h264_reader::nal::pps::PicParameterSet, Vec<u8>)> = None;

  let mut reader = h264_reader::annexb::AnnexBReader::accumulate(|h: h264_reader::nal::RefNal<'_>| {
    if !h.is_complete() {
      log::warn!("incomplete NAL unit found, buffering");
      return h264_reader::push::NalInterest::Buffer;
    }

    let mut buf_reader = h.reader();
    let Ok(reader_content) = io::BufRead::fill_buf(&mut buf_reader) else {
      log::warn!("unable to fill nal buffer with contents of current reader");

      return h264_reader::push::NalInterest::Buffer;
    };

    match h.header().map(h264_reader::nal::NalHeader::nal_unit_type) {
      Ok(h264_reader::nal::UnitType::SeqParameterSet) => {
        let sps = h264_reader::nal::sps::SeqParameterSet::from_bits(h.rbsp_bits());
        log::trace!("sps data = {sps:?}");

        if let Ok(s) = sps {
          found_sps = Some((s, reader_content.to_owned()));
        }
      }
      Ok(h264_reader::nal::UnitType::PicParameterSet) => {
        let mut ctx = h264_reader::Context::default();
        if let Some((s, _)) = found_sps.clone() {
          ctx.put_seq_param_set(s);
        }
        let pps = h264_reader::nal::pps::PicParameterSet::from_bits(&ctx, h.rbsp_bits());
        log::trace!("pps data = {pps:?}");

        if let Ok(p) = pps {
          found_pps = Some((p, reader_content.to_owned()));
        }
      }
      Ok(k) => log::debug!("uninteresting NAL type - {k:?}"),
      Err(error) => log::warn!("unable to fetch NAL type from header - {error:?}"),
    }

    h264_reader::push::NalInterest::Ignore
  });

  reader.push(&written_data);

  let ((sps, dec_sps), (_, dec_pps)) = found_sps
    .zip(found_pps)
    .ok_or_else(|| anyhow::Error::msg("codec private not found in bitstream"))?;

  log::info!(
    "generating advanced video coding configuration ({} bytes of sps & {} bytes of pps)",
    dec_sps.len(),
    dec_pps.len(),
  );

  private_data.push(1u8); // 1
  private_data.push(sps.profile_idc.into()); // 2
  private_data.push(sps.constraint_flags.into()); // 3
  private_data.push(sps.level_idc); // 4

  private_data.push(0b1111_1111); // 5
  private_data.push(0b1110_0001); // 6

  let d = u16::try_from(dec_sps.len())
    .with_context(|| "possible truncated sps length problem")?
    .to_be_bytes();

  private_data.extend_from_slice(&d);
  private_data.extend_from_slice(dec_sps.as_slice());
  private_data.push(1u8);

  let d = u16::try_from(dec_pps.len())
    .map(|v| v + 1u16)
    .with_context(|| "possible truncated pps length problem")?
    .to_be_bytes();

  private_data.extend_from_slice(&d);
  private_data.extend_from_slice(dec_pps.as_slice());
  private_data.push(0);

  // Now that we have our buffer of codec private data, validate it against the implementation from
  // the `h264_reader` crate, just to make sure we actually have something reasonable.
  match h264_reader::avcc::AvcDecoderConfigurationRecord::try_from(private_data.as_slice()) {
    Ok(record) => {
      let profile_idc = record.avc_profile_indication();
      log::info!("validated our avc decoder configuration profile: {profile_idc:?}");
    }
    Err(error) => log::warn!("no avc decoder config - {error:?}"),
  }

  Ok(private_data)
}
