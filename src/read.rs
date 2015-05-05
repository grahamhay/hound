// Hound -- A WAV encoding and decoding library in Rust
// Copyright (C) 2015 Ruud van Asseldonk
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License, version 3,
// as published by the Free Software Foundation.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::fs;
use std::io;
use std::marker;
use std::path;
use super::{Error, Result, Sample, WavSpec};

// TODO: Can this be unified among Hound and Claxon? Copy + Paste is bad, but
// I refuse to use an external crate just to read into an array of bytes, or
// to read an integer. Such functionality should really be in the standard
// library. Practically _every_ program that does IO will need more high-level
// functionality than what the standard library currently provides.
/// Extends the functionality of `io::Read` with additional methods.
///
/// The methods may be used on any type that implements `io::Read`.
pub trait ReadExt: io::Read {
    /// Reads as many bytes as `buf` is long.
    ///
    /// This may issue multiple `read` calls internally. An error is returned
    /// if `read` read 0 bytes before the buffer is full.
    fn read_into(&mut self, buf: &mut [u8]) -> io::Result<()>;

    /// Reads `n` bytes and returns them in a vector.
    fn read_bytes(&mut self, n: usize) -> io::Result<Vec<u8>>;

    /// Reads two bytes and interprets them as a little-endian 16-bit signed integer.
    fn read_le_i16(&mut self) -> io::Result<i16>;

    /// Reads two bytes and interprets them as a little-endian 16-bit unsigned integer.
    fn read_le_u16(&mut self) -> io::Result<u16>;

    /// Reads four bytes and interprets them as a little-endian 32-bit unsigned integer.
    fn read_le_u32(&mut self) -> io::Result<u32>;
}

impl<R> ReadExt for R where R: io::Read {
    fn read_into(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let mut n = 0;
        while n < buf.len() {
            let progress = try!(self.read(&mut buf[n ..]));
            if progress > 0 {
                n += progress;
            } else {
                return Err(io::Error::new(io::ErrorKind::Other,
                                          "Failed to read enough bytes."));
            }
        }
        Ok(())
    }

    fn read_bytes(&mut self, n: usize) -> io::Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(n);
        // TODO: is there a safe alternative that is not crazy like draining
        // a repeat(0u8) iterator?
        unsafe { buf.set_len(n); }
        try!(self.read_into(&mut buf[..]));
        Ok(buf)
    }

    fn read_le_i16(&mut self) -> io::Result<i16> {
        self.read_le_u16().map(|x| x as i16)
    }

    fn read_le_u16(&mut self) -> io::Result<u16> {
        let mut buf = [0u8; 2];
        try!(self.read_into(&mut buf));
        Ok((buf[1] as u16) << 8 | (buf[0] as u16))
    }

    fn read_le_u32(&mut self) -> io::Result<u32> {
        let mut buf = [0u8; 4];
        try!(self.read_into(&mut buf));
        Ok((buf[3] as u32) << 24 | (buf[2] as u32) << 16 |
           (buf[1] as u32) << 8  | (buf[0] as u32) << 0)
    }
}

/// The different chunks that a WAVE file can contain.
enum ChunkKind {
    Fmt,
    Data,
    Unknown
}

/// Describes the structure of a chunk in the WAVE file.
struct ChunkHeader {
    pub kind: ChunkKind,
    pub len: u32
}

/// Specifies properties of the audio data, as well as the layout of the stream.
#[derive(Clone, Copy)]
struct WavSpecEx {
    /// The normal information about the audio data.
    ///
    /// Bits per sample here is the number of _used_ bits per sample, not the
    /// number of bits used to _store_ a sample.
    spec: WavSpec,

    /// The number of bytes used to store a sample.
    bytes_per_sample: u16
}

/// A reader that reads the WAVE format from the underlying reader.
///
/// A `WavReader` is a streaming reader. It reads data from the underlying
/// reader on demand, and it reads no more than strictly necessary. No internal
/// buffering is performed on the underlying reader.
pub struct WavReader<R> {
    /// Specification of the file as found in the fmt chunk.
    spec: WavSpec,

    /// The number of bytes used to store a sample in the stream.
    bytes_per_sample: u16,

    /// The number of samples in the data chunk.
    ///
    /// The data chunk is limited to a 4 GiB length because its header has a
    /// 32-bit length field. A sample takes at least one byte to store, so the
    /// number of samples is always less than 2^32.
    num_samples: u32,

    /// The number of samples read so far.
    samples_read: u32,

    /// The reader from which the WAVE format is read.
    reader: R
}

/// An iterator that yields samples of type `S` read from a `WavReader`.
pub struct WavSamples<'wr, R, S> where R: 'wr {
    reader: &'wr mut WavReader<R>,
    phantom_sample: marker::PhantomData<S>
}

impl<R> WavReader<R> where R: io::Read {

    /// Reads the RIFF WAVE header, returns the supposed file size.
    fn read_wave_header(reader: &mut R) -> Result<u32> {
        // Every WAVE file starts with the four bytes 'RIFF' and a file length.
        // TODO: the old approach of having a slice on the stack and reading
        // into it is more cumbersome, but also avoids a heap allocation. Is
        // the compiler smart enough to avoid the heap allocation anyway? I
        // would not expect it to be.
        if b"RIFF" != &try!(reader.read_bytes(4))[..] {
            return Err(Error::FormatError("no RIFF tag found"));
        }

        // TODO: would this be useful anywhere? Probably not, except for
        // validating files, but do we need to be so strict?
        let file_len = try!(reader.read_le_u32());

        // Next four bytes indicate the file type, which should be WAVE.
        if b"WAVE" != &try!(reader.read_bytes(4))[..] {
            return Err(Error::FormatError("no WAVE tag found"));
        }

        Ok(file_len)
    }

    /// Attempts to read an 8-byte chunk header.
    fn read_chunk_header(reader: &mut R) -> Result<ChunkHeader> {
        let mut kind_str = [0; 4];
        try!(reader.read_into(&mut kind_str));
        let len = try!(reader.read_le_u32());

        let kind = match &kind_str[..] {
            b"fmt " => ChunkKind::Fmt,
            b"data" => ChunkKind::Data,
            _ => ChunkKind::Unknown
        };

        Ok(ChunkHeader { kind: kind, len: len })
    }

    /// Reads the fmt chunk of the file, returns the information it provides.
    fn read_fmt_chunk(reader: &mut R, chunk_len: u32) -> Result<WavSpecEx> {
        // A minimum chunk length of at least 16 is assumed. Note: actually,
        // the first 14 bytes contain enough information to fully specify the
        // file. I have not encountered a file with a 14-byte fmt section
        // though. If you ever encounter such file, please contact me.
        if chunk_len < 16 {
            return Err(Error::FormatError("invalid fmt chunk size"));
        }

        // Read the WAVEFORMAT struct, as defined at
        // https://msdn.microsoft.com/en-us/library/ms713498.aspx.
        // ```
        // typedef struct {
        //     WORD  wFormatTag;
        //     WORD  nChannels;
        //     DWORD nSamplesPerSec;
        //     DWORD nAvgBytesPerSec;
        //     WORD  nBlockAlign;
        // } WAVEFORMAT;
        // ```
        // The WAVEFORMATEX struct has two more members, as defined at
        // https://msdn.microsoft.com/en-us/library/ms713497.aspx
        // ```
        // typedef struct {
        //     WORD  wFormatTag;
        //     WORD  nChannels;
        //     DWORD nSamplesPerSec;
        //     DWORD nAvgBytesPerSec;
        //     WORD  nBlockAlign;
        //     WORD  wBitsPerSample;
        //     WORD  cbSize;
        // } WAVEFORMATEX;
        // ```
        // It appears that in either case, the minimal length of the fmt
        // section is 16 bytes, meaning that it does include the
        // `wBitsPerSample` field. (The name is misleading though, because it
        // is the number of bits used to store a sample, not all of the bits
        // need to be valid for all versions of the WAVE format.)
        let format_tag = try!(reader.read_le_u16());
        let n_channels = try!(reader.read_le_u16());
        let n_samples_per_sec = try!(reader.read_le_u32());
        let n_bytes_per_sec = try!(reader.read_le_u32());
        let block_align = try!(reader.read_le_u16());
        let bits_per_sample = try!(reader.read_le_u16());

        // Two of the stored fields are redundant, and may be ignored. We do
        // validate them to fail early for ill-formed files.
        if (bits_per_sample != block_align / n_channels * 8)
        || (n_bytes_per_sec != block_align as u32 * n_samples_per_sec) {
            return Err(Error::FormatError("inconsistent fmt chunk"));
        }

        // The bits per sample for a WAVEFORMAT struct is the number of bits
        // used to store a sample. Therefore, it must be a multiple of 8.
        if bits_per_sample % 8 != 0 {
           return Err(Error::FormatError("bits per sample is not a multiple of 8"));
        }

        let spec = WavSpec {
            channels: n_channels,
            sample_rate: n_samples_per_sec,
            bits_per_sample: bits_per_sample
        };

        // The different format tag definitions can be found in mmreg.h that is
        // part of the Windows SDK. The vast majority are esoteric vendor-
        // specific formats. We handle only a few. The following values could
        // be of interest:
        // 0x0001: WAVE_FORMAT_PCM
        // 0x0002: WAVE_FORMAT_ADPCM
        // 0x0003: WAVE_FORMAT_IEEE_FLOAT
        // 0xfffe: WAVE_FORMAT_EXTENSIBLE
        match format_tag {
            0x0001 => WavReader::<R>::read_wave_format_pcm(chunk_len, spec),
            0xfffe => WavReader::read_wave_format_extensible(reader, chunk_len, spec),
            _ => Err(Error::Unsupported)
        }
    }

    fn read_wave_format_pcm(chunk_len: u32, spec: WavSpec) -> Result<WavSpecEx> {
        // A WAVEFORMAT fmt block should be 16 bytes long. There could be two
        // extra bytes of `cbSize` set to 0, but I did not encounter such file
        // in practice. If you do, please contact me.
        if chunk_len != 16 {
            return Err(Error::FormatError("unexpected fmt chunk size"));
        }

        // For WAVE_FORMAT_PCM, only 8 or 16 bits per sample are valid
        // according to https://msdn.microsoft.com/en-us/library/ms713497.aspx.
        if spec.bits_per_sample != 8 && spec.bits_per_sample != 16 {
            return Err(Error::FormatError("bits per sample is not 8 or 16"));
        }

        let spec_ex = WavSpecEx {
            spec: spec,
            bytes_per_sample: spec.bits_per_sample / 8
        };
        Ok(spec_ex)
    }

    fn read_wave_format_extensible(mut reader: R, chunk_len: u32, spec: WavSpec)
                                   -> Result<WavSpecEx> {
        // 16 bytes were read already, there must be two more for the `cbSize`
        // field, and `cbSize` itself must be at least 22, so the chunk length
        // must be at least 40.
        if chunk_len < 40 {
            return Err(Error::FormatError("unexpected fmt chunk size"));
        }

        // `cbSize` is the last field of the WAVEFORMATEX struct.
        let cb_size = try!(reader.read_le_u16());

        // `cbSize` must be at least 22, but in this case we assume that it is
        // 22, because we would not know how to handle extra data anyway.
        if cb_size != 22 {
            return Err(Error::FormatError("unexpected WAVEFORMATEXTENSIBLE size"));
        }

        // What follows is the rest of the `WAVEFORMATEXTENSIBLE` struct, as
        // defined at https://msdn.microsoft.com/en-us/library/ms713496.aspx.
        // ```
        // typedef struct {
        //   WAVEFORMATEX  Format;
        //   union {
        //     WORD  wValidBitsPerSample;
        //     WORD  wSamplesPerBlock;
        //     WORD  wReserved;
        //   } Samples;
        //   DWORD   dwChannelMask;
        //   GUID    SubFormat;
        // } WAVEFORMATEXTENSIBLE, *PWAVEFORMATEXTENSIBLE;
        // ```
        let valid_bits_per_sample = try!(reader.read_le_u16());
        let _channel_mask = try!(reader.read_le_u32()); // Not used for now.
        let mut subformat = [0u8; 16];
        try!(reader.read_into(&mut subformat));

        // Several GUIDS are defined. At the moment, only KSDATAFORMAT_SUBTYPE_PCM
        // is supported.
        // TODO: Extract all these constants to a common place.
        if subformat != [0x01, 0x00, 0x00, 0x00,
                         0x00, 0x00, 0x10, 0x00,
                         0x80, 0x00, 0x00, 0xaa,
                         0x00, 0x38, 0x9b, 0x71] {
            return Err(Error::Unsupported);
        }

        let spec_ex = WavSpecEx {
            spec: WavSpec {
                bits_per_sample: valid_bits_per_sample,
                .. spec
            },
            bytes_per_sample: spec.bits_per_sample / 8
        };
        Ok(spec_ex)
    }

    /// Reads chunks until a data chunk is encountered.
    ///
    /// Returns the information from the fmt chunk and the length of the data
    /// chunk in bytes. Afterwards, the reader will be positioned at the first
    /// content byte of the data chunk.
    fn read_until_data(mut reader: R) -> Result<(WavSpecEx, u32)> {
        let mut spec_opt = None;

        loop {
            let header = try!(WavReader::read_chunk_header(&mut reader));
            match header.kind {
                ChunkKind::Fmt => {
                    let spec = try!(WavReader::read_fmt_chunk(&mut reader,
                                                              header.len));
                    spec_opt = Some(spec);
                },
                ChunkKind::Data => {
                    // The "fmt" chunk must precede the "data" chunk. Any
                    // chunks that come after the data chunk will be ignored.
                    if let Some(spec) = spec_opt {
                        return Ok((spec, header.len));
                    } else {
                        return Err(Error::FormatError("missing fmt chunk"));
                    }
                },
                ChunkKind::Unknown => {
                    // Ignore the chunk; skip all of its bytes.
                    // TODO: this could be more efficient by not allocating
                    // space on the heap, reading into it and then dropping it
                    // without use. For now, this solution is simplest. If Seek
                    // is supported we could skip, but that is a stronger bound
                    // than what is required ...
                    try!(reader.read_bytes(header.len as usize));
                }
            }
            // If no data chunk is ever encountered, the function will return
            // via one of the try! macros that return an Err on end of file.
        }
    }

    /// Attempts to create a reader that reads the WAVE format.
    ///
    /// The header is read immediately. Reading the data will be done on
    /// demand.
    pub fn new(mut reader: R) -> Result<WavReader<R>> {
        try!(WavReader::read_wave_header(&mut reader));
        let (spec_ex, data_len) = try!(WavReader::read_until_data(&mut reader));

        let num_samples = data_len / spec_ex.bytes_per_sample as u32;

        // The number of samples must be a multiple of the number of channels,
        // otherwise the last inter-channel sample would not have data for all
        // channels.
        if num_samples % spec_ex.spec.channels as u32 != 0 {
            return Err(Error::FormatError("invalid data chunk length"));
        }

        let wav_reader = WavReader {
            spec: spec_ex.spec,
            bytes_per_sample: spec_ex.bytes_per_sample,
            num_samples: num_samples,
            samples_read: 0,
            reader: reader
        };

        Ok(wav_reader)
    }

    // TODO: Should this return by value instead? A reference is more consistent
    // with Claxon, but the type is only 80 bits, barely larger than a pointer.
    // Is it worth the extra indirection? On the other hand, the indirection
    // is probably optimised away.
    /// Returns information about the WAVE file.
    pub fn spec(&self) -> &WavSpec {
        &self.spec
    }

    /// Returns an iterator over all samples.
    ///
    /// The channel data is is interleaved. The iterator is streaming. That is,
    /// if you call this method once, read a few samples, and call this method
    /// again, the second iterator will not start again from the beginning of
    /// the file, it will continue where the first iterator stopped.
    pub fn samples<'wr, S: Sample>(&'wr mut self) -> WavSamples<'wr, R, S> {
        WavSamples {
            reader: self,
            phantom_sample: marker::PhantomData
        }
    }

    /// Returns the duration of the file in samples.
    ///
    /// The duration is independent of the number of channels. It is expressed
    /// in units of samples. The duration in seconds can be obtained by
    /// dividing this number by the sample rate. The duration is independent of
    /// how many samples have been read already.
    pub fn duration(&self) -> u32 {
        self.num_samples / self.spec.channels as u32
    }

    /// Returns the number of values that the sample iterator will yield.
    ///
    /// The length of the file is its duration (in samples) times the number of
    /// channels. The length is independent of how many samples have been read
    /// already.
    pub fn len(&self) -> u32 {
        self.num_samples
    }
}

impl WavReader<io::BufReader<fs::File>> {
    /// Attempts to create a reader that reads from the specified file.
    ///
    /// This is a convenience constructor that opens a `File`, wraps it in a
    /// `BufReader` and then constructs a `WavReader` from it.
    pub fn open<P: AsRef<path::Path>>(filename: P)
                -> Result<WavReader<io::BufReader<fs::File>>> {
        let file = try!(fs::File::open(filename));
        let buf_reader = io::BufReader::new(file);
        WavReader::new(buf_reader)
    }
}

impl<'wr, R, S> Iterator for WavSamples<'wr, R, S>
where R: io::Read,
      S: Sample {
    type Item = Result<S>;

    fn next(&mut self) -> Option<Result<S>> {
        let reader = &mut self.reader;
        if reader.samples_read < reader.num_samples {
            reader.samples_read += 1;
            // TODO: determine how many bytes to read from reader.bytes_per_sample,
            // not from the Sample type itself.
            let sample = Sample::read(&mut reader.reader,
                                      reader.spec.bits_per_sample);
            Some(sample.map_err(Error::from))
        } else {
            None
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let samples_left = self.reader.num_samples - self.reader.samples_read;
        (samples_left as usize, Some(samples_left as usize))
    }
}

#[test]
fn duration_and_len_agree() {
    // TODO: add test samples with more channels.
    let files = &["testsamples/waveformat-16bit-44100Hz-mono.wav"];

    for fname in files {
        let reader = WavReader::open(fname).unwrap();
        assert_eq!(reader.spec().channels as u32 * reader.duration(),
                   reader.len());
    }
}

/// Tests reading the most basic wav file, one with only a WAVEFORMAT struct.
#[test]
fn read_wav_waveformat() {
    use std::fs;
    
    let file = fs::File::open("testsamples/waveformat-16bit-44100Hz-mono.wav")
                        .ok().expect("failed to open file");
    let buf_reader = io::BufReader::new(file);
    let mut wav_reader = WavReader::new(buf_reader)
                                   .ok().expect("failed to read header");

    assert_eq!(wav_reader.spec().channels, 1);
    assert_eq!(wav_reader.spec().sample_rate, 44100);
    assert_eq!(wav_reader.spec().bits_per_sample, 16);

    let samples: Vec<i16> = wav_reader.samples()
                                      .map(|r| r.ok().unwrap())
                                      .collect();

    // The test file has been prepared with these exact four samples.
    assert_eq!(&samples[..], &[2, -3, 5, -7]);
}

#[test]
fn read_wav_waveformat_ex() {
    // TODO: add a test sample that uses WAVEFORMATEX and verify that it can be
    // read properly.
}

#[test]
fn read_wav_waveformat_extensible() {
    // TODO: add a test sample that uses WAVEFORMATEXTENSIBLE (as produced by
    // Hound itself actually, so this should not be too hard), and verify that
    // it can be read properly.
}