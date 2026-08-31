//! Minimal streaming fragmented-MP4 muxer for Bilibili DASH tracks.
//!
//! Bilibili serves video and audio as separate single-track fMP4 files. Both
//! files consist of `ftyp + moov + sidx + (moof + mdat)*`. We build a new
//! two-track initialization segment, assign track id 2 to audio, then emit the
//! original fragments in decode-time order while patching only `mfhd` and
//! `tfhd`. Sample bytes are never decoded or copied through a codec.

use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt};
use std::io;
use std::ops::Range;
use std::pin::Pin;
use tokio::sync::mpsc;

const MAX_BOX_SIZE: usize = 64 * 1024 * 1024;
const VIDEO_TRACK_ID: u32 = 1;
const AUDIO_TRACK_ID: u32 = 2;

pub type MuxByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>;

pub struct MuxedMp4 {
    pub stream: MuxByteStream,
    pub content_length: Option<u64>,
}

type UpstreamStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

struct BoxReader {
    stream: UpstreamStream,
    buffered: BytesMut,
    eof: bool,
}

impl BoxReader {
    async fn open(client: &reqwest::Client, url: &str) -> io::Result<Self> {
        let response = client
            .get(url)
            .header("User-Agent", "Mozilla/5.0")
            .header("Referer", "https://www.bilibili.com/")
            .send()
            .await
            .map_err(io::Error::other)?
            .error_for_status()
            .map_err(io::Error::other)?;
        Ok(Self {
            stream: Box::pin(response.bytes_stream()),
            buffered: BytesMut::new(),
            eof: false,
        })
    }

    async fn next_box(&mut self) -> io::Result<Option<Bytes>> {
        loop {
            if self.buffered.len() >= 8 {
                let header_len = if read_u32(&self.buffered, 0)? == 1 {
                    16
                } else {
                    8
                };
                if self.buffered.len() >= header_len {
                    let size = box_size_prefix(&self.buffered)?;
                    if size < header_len || size > MAX_BOX_SIZE {
                        return Err(invalid(format!("invalid fMP4 box size {size}")));
                    }
                    if self.buffered.len() >= size {
                        return Ok(Some(self.buffered.split_to(size).freeze()));
                    }
                }
            }

            if self.eof {
                if self.buffered.is_empty() {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated fMP4 box",
                ));
            }
            match self.stream.next().await {
                Some(Ok(chunk)) => self.buffered.extend_from_slice(&chunk),
                Some(Err(err)) => return Err(io::Error::other(err)),
                None => self.eof = true,
            }
        }
    }
}

struct TrackSource {
    reader: BoxReader,
    moov: Bytes,
    pending_moof: Option<Bytes>,
    referenced_bytes: Option<u64>,
}

impl TrackSource {
    async fn open(client: &reqwest::Client, url: &str) -> io::Result<(Bytes, Self)> {
        let mut reader = BoxReader::open(client, url).await?;
        let mut ftyp = None;
        let mut moov = None;
        let mut pending_moof = None;
        let mut referenced_bytes = None;

        for _ in 0..32 {
            let Some(atom) = reader.next_box().await? else {
                break;
            };
            match atom_type(&atom)? {
                b"ftyp" => ftyp = Some(atom),
                b"moov" => moov = Some(atom),
                b"sidx" => referenced_bytes = parse_sidx_referenced_bytes(&atom).ok(),
                b"moof" => {
                    pending_moof = Some(atom);
                    break;
                }
                _ => {}
            }
        }

        let ftyp = ftyp.ok_or_else(|| invalid("fMP4 source has no ftyp"))?;
        let moov = moov.ok_or_else(|| invalid("fMP4 source has no moov"))?;
        Ok((
            ftyp,
            Self {
                reader,
                moov,
                pending_moof,
                referenced_bytes,
            },
        ))
    }

    async fn next_fragment(&mut self) -> io::Result<Option<Fragment>> {
        let moof = if let Some(moof) = self.pending_moof.take() {
            moof
        } else {
            loop {
                let Some(atom) = self.reader.next_box().await? else {
                    return Ok(None);
                };
                if atom_type(&atom)? == b"moof" {
                    break atom;
                }
            }
        };
        let decode_time = fragment_decode_time(&moof)?;
        loop {
            let atom =
                self.reader.next_box().await?.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "moof without mdat")
                })?;
            if atom_type(&atom)? == b"mdat" {
                return Ok(Some(Fragment {
                    moof,
                    mdat: atom,
                    decode_time,
                }));
            }
        }
    }
}

struct Fragment {
    moof: Bytes,
    mdat: Bytes,
    decode_time: u64,
}

/// Start an online mux of two Bilibili DASH fMP4 tracks.
pub async fn mux_dash(
    client: &reqwest::Client,
    video_url: &str,
    audio_url: &str,
) -> io::Result<MuxedMp4> {
    let ((video_ftyp, mut video), (_audio_ftyp, mut audio)) = tokio::try_join!(
        TrackSource::open(client, video_url),
        TrackSource::open(client, audio_url),
    )?;

    let video_timescale = track_timescale(&video.moov)?;
    let audio_timescale = track_timescale(&audio.moov)?;
    let init = merge_initialization(&video_ftyp, &video.moov, &audio.moov)?;
    let content_length = video
        .referenced_bytes
        .zip(audio.referenced_bytes)
        .map(|(v, a)| init.len() as u64 + v + a);

    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(4);
    tokio::spawn(async move {
        if tx.send(Ok(init)).await.is_err() {
            return;
        }
        let result = mux_fragments(
            &tx,
            &mut video,
            &mut audio,
            video_timescale,
            audio_timescale,
        )
        .await;
        if let Err(err) = result {
            let _ = tx.send(Err(err)).await;
        }
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    Ok(MuxedMp4 {
        stream: Box::pin(stream),
        content_length,
    })
}

async fn mux_fragments(
    tx: &mpsc::Sender<Result<Bytes, io::Error>>,
    video: &mut TrackSource,
    audio: &mut TrackSource,
    video_timescale: u32,
    audio_timescale: u32,
) -> io::Result<()> {
    let mut next_video = video.next_fragment().await?;
    let mut next_audio = audio.next_fragment().await?;
    let mut sequence = 1u32;

    while next_video.is_some() || next_audio.is_some() {
        let take_video = match (&next_video, &next_audio) {
            (Some(v), Some(a)) => {
                (v.decode_time as u128) * (audio_timescale as u128)
                    <= (a.decode_time as u128) * (video_timescale as u128)
            }
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };

        let (mut fragment, track_id) = if take_video {
            (next_video.take().expect("checked"), VIDEO_TRACK_ID)
        } else {
            (next_audio.take().expect("checked"), AUDIO_TRACK_ID)
        };
        fragment.moof = patch_moof(&fragment.moof, track_id, sequence)?;
        sequence = sequence.wrapping_add(1);
        if tx.send(Ok(fragment.moof)).await.is_err() || tx.send(Ok(fragment.mdat)).await.is_err() {
            return Ok(());
        }

        if take_video {
            next_video = video.next_fragment().await?;
        } else {
            next_audio = audio.next_fragment().await?;
        }
    }
    Ok(())
}

fn merge_initialization(
    video_ftyp: &Bytes,
    video_moov: &Bytes,
    audio_moov: &Bytes,
) -> io::Result<Bytes> {
    let mut mvhd = child(video_moov, b"mvhd")?.to_vec();
    let mut video_trak = child(video_moov, b"trak")?.to_vec();
    let mut audio_trak = child(audio_moov, b"trak")?.to_vec();
    let video_mvex = child(video_moov, b"mvex")?;
    let audio_mvex = child(audio_moov, b"mvex")?;
    let mut video_trex = child(video_mvex, b"trex")?.to_vec();
    let mut audio_trex = child(audio_mvex, b"trex")?.to_vec();

    patch_mvhd_next_track_id(&mut mvhd, 3)?;
    patch_trak_track_id(&mut video_trak, VIDEO_TRACK_ID)?;
    patch_trak_track_id(&mut audio_trak, AUDIO_TRACK_ID)?;
    patch_full_box_u32(&mut video_trex, 12, VIDEO_TRACK_ID)?;
    patch_full_box_u32(&mut audio_trex, 12, AUDIO_TRACK_ID)?;

    let mvex = make_box(b"mvex", &[&video_trex, &audio_trex])?;
    let moov = make_box(b"moov", &[&mvhd, &video_trak, &audio_trak, &mvex])?;
    let mut init = BytesMut::with_capacity(video_ftyp.len() + moov.len());
    init.extend_from_slice(video_ftyp);
    init.extend_from_slice(&moov);
    Ok(init.freeze())
}

fn patch_moof(moof: &Bytes, track_id: u32, sequence: u32) -> io::Result<Bytes> {
    let mut out = moof.to_vec();
    let mfhd = child_range(&out, 0..out.len(), b"mfhd")?;
    patch_at(&mut out, mfhd.start + 12, sequence)?;
    let traf = child_range(&out, 0..out.len(), b"traf")?;
    let tfhd = child_range(&out, traf, b"tfhd")?;
    patch_at(&mut out, tfhd.start + 12, track_id)?;
    Ok(Bytes::from(out))
}

fn fragment_decode_time(moof: &[u8]) -> io::Result<u64> {
    let traf = child_range(moof, 0..moof.len(), b"traf")?;
    let tfdt = child_range(moof, traf, b"tfdt")?;
    let version = *moof
        .get(tfdt.start + 8)
        .ok_or_else(|| invalid("short tfdt"))?;
    if version == 1 {
        read_u64(moof, tfdt.start + 12)
    } else {
        Ok(read_u32(moof, tfdt.start + 12)? as u64)
    }
}

fn track_timescale(moov: &[u8]) -> io::Result<u32> {
    let trak = child_range(moov, 0..moov.len(), b"trak")?;
    let mdia = child_range(moov, trak, b"mdia")?;
    let mdhd = child_range(moov, mdia, b"mdhd")?;
    let version = *moov
        .get(mdhd.start + 8)
        .ok_or_else(|| invalid("short mdhd"))?;
    let offset = if version == 1 {
        mdhd.start + 28
    } else {
        mdhd.start + 20
    };
    let timescale = read_u32(moov, offset)?;
    if timescale == 0 {
        Err(invalid("zero track timescale"))
    } else {
        Ok(timescale)
    }
}

fn patch_trak_track_id(trak: &mut [u8], track_id: u32) -> io::Result<()> {
    let tkhd = child_range(trak, 0..trak.len(), b"tkhd")?;
    let version = *trak
        .get(tkhd.start + 8)
        .ok_or_else(|| invalid("short tkhd"))?;
    let offset = if version == 1 {
        tkhd.start + 28
    } else {
        tkhd.start + 20
    };
    patch_at(trak, offset, track_id)
}

fn patch_mvhd_next_track_id(mvhd: &mut [u8], next_track_id: u32) -> io::Result<()> {
    if mvhd.len() < 12 {
        return Err(invalid("short mvhd"));
    }
    patch_at(mvhd, mvhd.len() - 4, next_track_id)
}

fn patch_full_box_u32(atom: &mut [u8], offset: usize, value: u32) -> io::Result<()> {
    patch_at(atom, offset, value)
}

fn patch_at(data: &mut [u8], offset: usize, value: u32) -> io::Result<()> {
    let dst = data
        .get_mut(offset..offset + 4)
        .ok_or_else(|| invalid("box patch out of bounds"))?;
    dst.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn child<'a>(parent: &'a [u8], wanted: &[u8; 4]) -> io::Result<&'a [u8]> {
    let range = child_range(parent, 0..parent.len(), wanted)?;
    Ok(&parent[range])
}

fn child_range(data: &[u8], parent: Range<usize>, wanted: &[u8; 4]) -> io::Result<Range<usize>> {
    let parent_header = box_header_len(&data[parent.clone()])?;
    let mut offset = parent.start + parent_header;
    while offset + 8 <= parent.end {
        let size = box_size_prefix(&data[offset..parent.end])?;
        let end = offset
            .checked_add(size)
            .ok_or_else(|| invalid("box size overflow"))?;
        if end > parent.end {
            return Err(invalid("child box exceeds parent"));
        }
        if data.get(offset + 4..offset + 8) == Some(wanted.as_slice()) {
            return Ok(offset..end);
        }
        offset = end;
    }
    Err(invalid(format!(
        "missing {} box",
        String::from_utf8_lossy(wanted)
    )))
}

fn make_box(kind: &[u8; 4], children: &[&[u8]]) -> io::Result<Vec<u8>> {
    let payload: usize = children.iter().map(|c| c.len()).sum();
    let size = 8usize
        .checked_add(payload)
        .ok_or_else(|| invalid("box size overflow"))?;
    let size32 = u32::try_from(size).map_err(|_| invalid("initialization box too large"))?;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&size32.to_be_bytes());
    out.extend_from_slice(kind);
    for child in children {
        out.extend_from_slice(child);
    }
    Ok(out)
}

fn parse_sidx_referenced_bytes(sidx: &[u8]) -> io::Result<u64> {
    let header = box_header_len(sidx)?;
    let version = *sidx.get(header).ok_or_else(|| invalid("short sidx"))?;
    let mut offset = header + 4 + 4 + 4;
    offset += if version == 0 { 8 } else { 16 };
    offset += 2;
    let count = read_u16(sidx, offset)? as usize;
    offset += 2;
    let mut total = 0u64;
    for _ in 0..count {
        let reference = read_u32(sidx, offset)?;
        if reference >> 31 != 0 {
            return Err(invalid("nested sidx reference unsupported"));
        }
        total += (reference & 0x7fff_ffff) as u64;
        offset += 12;
    }
    Ok(total)
}

fn atom_type(data: &[u8]) -> io::Result<&[u8]> {
    data.get(4..8).ok_or_else(|| invalid("short box header"))
}

fn box_header_len(data: &[u8]) -> io::Result<usize> {
    Ok(if read_u32(data, 0)? == 1 { 16 } else { 8 })
}

fn box_size_prefix(data: &[u8]) -> io::Result<usize> {
    let size32 = read_u32(data, 0)?;
    if size32 == 0 {
        return Err(invalid("box extending to EOF unsupported"));
    }
    if size32 == 1 {
        usize::try_from(read_u64(data, 8)?).map_err(|_| invalid("box too large"))
    } else {
        Ok(size32 as usize)
    }
}

fn read_u16(data: &[u8], offset: usize) -> io::Result<u16> {
    let b: [u8; 2] = data
        .get(offset..offset + 2)
        .ok_or_else(|| invalid("short u16"))?
        .try_into()
        .unwrap();
    Ok(u16::from_be_bytes(b))
}

fn read_u32(data: &[u8], offset: usize) -> io::Result<u32> {
    let b: [u8; 4] = data
        .get(offset..offset + 4)
        .ok_or_else(|| invalid("short u32"))?
        .try_into()
        .unwrap();
    Ok(u32::from_be_bytes(b))
}

fn read_u64(data: &[u8], offset: usize) -> io::Result<u64> {
    let b: [u8; 8] = data
        .get(offset..offset + 8)
        .ok_or_else(|| invalid("short u64"))?
        .try_into()
        .unwrap();
    Ok(u64::from_be_bytes(b))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn makes_and_finds_boxes() {
        let child_a = make_box(b"aaaa", &[]).unwrap();
        let child_b = make_box(b"bbbb", &[]).unwrap();
        let parent = make_box(b"test", &[&child_a, &child_b]).unwrap();
        assert_eq!(child(&parent, b"bbbb").unwrap(), child_b);
    }

    #[test]
    fn patches_fragment_ids_without_changing_size() {
        let mfhd = make_box(b"mfhd", &[&[0, 0, 0, 0, 0, 0, 0, 9]]).unwrap();
        let tfhd = make_box(b"tfhd", &[&[0, 0, 0, 0, 0, 0, 0, 1]]).unwrap();
        let traf = make_box(b"traf", &[&tfhd]).unwrap();
        let moof = Bytes::from(make_box(b"moof", &[&mfhd, &traf]).unwrap());
        let patched = patch_moof(&moof, 2, 7).unwrap();
        assert_eq!(patched.len(), moof.len());
        let mfhd = child_range(&patched, 0..patched.len(), b"mfhd").unwrap();
        let traf = child_range(&patched, 0..patched.len(), b"traf").unwrap();
        let tfhd = child_range(&patched, traf, b"tfhd").unwrap();
        assert_eq!(read_u32(&patched, mfhd.start + 12).unwrap(), 7);
        assert_eq!(read_u32(&patched, tfhd.start + 12).unwrap(), 2);
    }

    /// Full network fixture used manually before releases. The output stays in
    /// target/ and is verified with ffprobe by the release workflow/operator.
    #[tokio::test]
    #[ignore = "downloads the complete public Bilibili test video"]
    async fn live_mux_target_video() {
        let media = crate::bilibili_parser::get_bilibili_media("BV1dqj16aECG", Some(0), 80)
            .await
            .unwrap();
        let crate::bilibili_parser::BilibiliMedia::Dash {
            video_url,
            audio_url,
            ..
        } = media
        else {
            panic!("expected DASH");
        };
        let client = reqwest::Client::builder().build().unwrap();
        let muxed = mux_dash(&client, &video_url, &audio_url).await.unwrap();
        let expected = muxed.content_length;
        let path = "target/bilibili-1080-mux-test.mp4";
        let mut file = tokio::fs::File::create(path).await.unwrap();
        let mut stream = muxed.stream;
        let mut written = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.unwrap();
            file.write_all(&chunk).await.unwrap();
            written += chunk.len() as u64;
        }
        file.flush().await.unwrap();
        if let Some(expected) = expected {
            assert_eq!(written, expected);
        }
        assert!(written > 10 * 1024 * 1024);
    }
}
