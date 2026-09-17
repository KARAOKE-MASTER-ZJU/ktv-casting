//! Offline inspection tool; no network or codec dependencies.
//! cargo run --example flatten_mp4 -- input.mp4 output.mp4
use ktv_casting_lib::seekable_mp4::{Mp4Builder, ReadPart};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: flatten_mp4 INPUT OUTPUT (new file)".into());
    }
    let mut input = File::open(&args[1])?;
    let size = input.metadata()?.len();
    let mut ftyp = None;
    let mut builder = None;
    let mut metadata_bytes = 0;
    let started = std::time::Instant::now();
    while input.stream_position()? < size {
        let offset = input.stream_position()?;
        let mut h = [0; 8];
        input.read_exact(&mut h)?;
        let mut atom_header = h.to_vec();
        let atom_size = match u32::from_be_bytes(h[..4].try_into()?) {
            0 => size - offset,
            1 => {
                let mut extended = [0; 8];
                input.read_exact(&mut extended)?;
                atom_header.extend_from_slice(&extended);
                u64::from_be_bytes(extended)
            }
            n => n as u64,
        };
        if atom_size < atom_header.len() as u64 || atom_size > size - offset {
            return Err("invalid atom size".into());
        }
        if matches!(&h[4..8], b"ftyp" | b"moov" | b"moof") {
            if atom_size > 16 * 1024 * 1024 {
                return Err("metadata exceeds limit".into());
            }
            atom_header.resize(atom_size as usize, 0);
            let header_size = if h[..4] == 1u32.to_be_bytes() { 16 } else { 8 };
            input.read_exact(&mut atom_header[header_size..])?;
            metadata_bytes += atom_size;
            match &h[4..8] {
                b"ftyp" => ftyp = Some(atom_header),
                b"moov" => {
                    builder = Some(Mp4Builder::new(
                        ftyp.as_deref().ok_or("missing ftyp")?,
                        &atom_header,
                        0,
                    )?)
                }
                b"moof" => {
                    builder
                        .as_mut()
                        .ok_or("missing moov")?
                        .add_fragment(0, offset, &atom_header)?
                }
                _ => unreachable!(),
            }
        }
        input.seek(SeekFrom::Start(offset + atom_size))?;
    }
    let mp4 = builder.ok_or("missing metadata")?.finish()?;
    eprintln!(
        "index_ms={} metadata_bytes={} header_bytes={} output_bytes={}",
        started.elapsed().as_millis(),
        metadata_bytes,
        mp4.prefix.len(),
        mp4.len
    );
    let mut output = File::options()
        .write(true)
        .create_new(true)
        .open(&args[2])?;
    for part in mp4.read_plan(0..mp4.len)? {
        match part {
            ReadPart::Header(range) => output.write_all(&mp4.prefix[range])?,
            ReadPart::Source { range, .. } => {
                input.seek(SeekFrom::Start(range.start))?;
                let length = range.end - range.start;
                if io::copy(&mut (&mut input).take(length), &mut output)? != length {
                    return Err("short source read".into());
                }
            }
        }
    }
    Ok(())
}
