//! Support for RLE Lossless image decoding.
//!
//! implementation taken from Pydicom:
//! <https://github.com/pydicom/pydicom/blob/master/pydicom/pixel_data_handlers/rle_handler.py>
//!
//! Copyright 2008-2021 pydicom authors.
//!
//! License: <https://github.com/pydicom/pydicom/blob/master/LICENSE>
use byteordered::byteorder::{ByteOrder, LittleEndian};
use snafu::{whatever, OptionExt, ResultExt};

use crate::adapters::{DecodeResult, PixelDataObject, PixelRWAdapter};
use std::io::{self, Read, Seek};

use super::MissingAttributeSnafu;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RLELosslessAdapter;

/// Decode TS: 1.2.840.10008.1.2.5 (RLE Lossless)
impl PixelRWAdapter for RLELosslessAdapter {
    /// Decode the DICOM image from RLE Lossless completely.
    ///
    /// See <http://dicom.nema.org/medical/Dicom/2018d/output/chtml/part05/chapter_G.html>
    fn decode(&self, src: &dyn PixelDataObject, dst: &mut Vec<u8>) -> DecodeResult<()> {
        let cols = src
            .cols()
            .context(MissingAttributeSnafu { name: "Columns" })?;
        let rows = src.rows().context(MissingAttributeSnafu { name: "Rows" })?;
        let samples_per_pixel = src.samples_per_pixel().context(MissingAttributeSnafu {
            name: "SamplesPerPixel",
        })?;
        let bits_allocated = src.bits_allocated().context(MissingAttributeSnafu {
            name: "BitsAllocated",
        })?;

        if bits_allocated != 8 && bits_allocated != 16 {
            whatever!("BitsAllocated other than 8 or 16 is not supported");
        }
        // For RLE the number of fragments = number of frames
        // therefore, we can fetch the fragments one-by-one
        let nr_frames =
            src.number_of_fragments()
                .whatever_context("Invalid pixel data, no fragments found")? as usize;
        let bytes_per_sample = bits_allocated / 8;
        // `stride` it the total number of bytes for each sample plane
        let stride: usize = bytes_per_sample as usize * cols as usize * rows as usize;
        let frame_size = samples_per_pixel as usize * stride;
        dst.resize(frame_size * nr_frames, 0);

        // RLE encoded data is ordered like this (for 16-bit, 3 sample):
        //  Segment: 0     | 1     | 2     | 3     | 4     | 5
        //           R MSB | R LSB | G MSB | G LSB | B MSB | B LSB
        //  A segment contains only the MSB or LSB parts of all the sample pixels

        // To minimise the amount of array manipulation later, and to make things
        // faster we interleave each segment in a manner consistent with a planar
        // configuration of 1 (and use little endian byte ordering):
        //    All red samples             | All green samples           | All blue
        //    Pxl 1   Pxl 2   ... Pxl N   | Pxl 1   Pxl 2   ... Pxl N   | ...
        //    LSB MSB LSB MSB ... LSB MSB | LSB MSB LSB MSB ... LSB MSB | ...

        for i in 0..nr_frames {
            let fragment = &src
                .fragment(i)
                .whatever_context("No pixel data found for frame")?;
            let mut offsets = read_rle_header(fragment);
            offsets.push(fragment.len() as u32);

            for sample_number in 0..samples_per_pixel {
                for byte_offset in (0..bytes_per_sample).rev() {
                    // ii is 1, 0, 3, 2, 5, 4 for the example above
                    // This is where the segment order correction occurs
                    let ii = sample_number * bytes_per_sample + byte_offset;
                    let segment = &fragment
                        [offsets[ii as usize] as usize..offsets[(ii + 1) as usize] as usize];
                    let buff = io::Cursor::new(segment);
                    let mut decoded_segment: Vec<u8> = vec![0; rows as usize * cols as usize];
                    let decode_length = decode_rle_segment(buff, &mut decoded_segment)
                        .map_err(|e| Box::new(e) as Box<_>)
                        .whatever_context("Failed to read RLE segments")?;

                    assert_eq!(decode_length, decoded_segment.len());

                    // Interleave pixels as described in the example above
                    let byte_offset = bytes_per_sample - byte_offset - 1;
                    let sample_offset = (sample_number * bytes_per_sample) as usize;

                    let start = frame_size * i
                        + sample_offset
                        + byte_offset as usize;
                    let end = frame_size * (i + 1);
                    for (decoded_index, dst_index) in
                        (start..end).step_by(bytes_per_sample as usize * samples_per_pixel as usize).enumerate()
                    {
                        dst[dst_index] = decoded_segment[decoded_index];
                    }
                }
            }
        }
        Ok(())
    }

    // TODO(#125) implement `encode`
}

// Read the RLE header and return the offsets
fn read_rle_header(fragment: &[u8]) -> Vec<u32> {
    let nr_segments = LittleEndian::read_u32(&fragment[0..4]);
    let mut offsets = vec![0; nr_segments as usize];
    LittleEndian::read_u32_into(&fragment[4..4 * (nr_segments + 1) as usize], &mut offsets);
    offsets
}

fn decode_rle_segment<R: Read + Seek>(mut reader: R, target_buffer: &mut Vec<u8>) -> io::Result<usize> {
    let mut pos: usize = 0;
    let mut header: [u8; 1] = [0];
    let mut data: [u8; 1] = [0];
    while pos < target_buffer.len() {
        reader.read_exact(&mut header)?;
        let h = header[0] as i8;
        if (-127..=-1).contains(&h) {
            let new_pos = pos + (1 - h as isize) as usize;
            reader.read_exact(&mut data)?;
            for i in pos .. new_pos {
                target_buffer[i] = data[0];
            }
            pos = new_pos;
        } else if h >= 0 {
            let num_vals = h as usize + 1;
            reader.read_exact(&mut target_buffer[pos..pos + num_vals])?;
            pos += num_vals;
        } else {
            // h = -128 is a no-op.
        }
    }
    Ok(pos)

}

/// PackBits Reader from the image-tiff crate
/// Copyright 2018-2021 PistonDevelopers.
/// License: <https://github.com/image-rs/image-tiff/blob/master/LICENSE>
/// From: https://github.com/image-rs/image-tiff/blob/master/src/decoder/stream.rs
#[derive(Debug)]
pub struct PackBitsReader {
    buffer: io::Cursor<Vec<u8>>,
}

impl PackBitsReader {
    /// Wraps a reader
    pub fn new<R: Read + Seek>(
        mut reader: R,
        length: usize,
    ) -> io::Result<(usize, PackBitsReader)> {
        let mut buffer = Vec::new();
        let mut header: [u8; 1] = [0];
        let mut data: [u8; 1] = [0];

        let mut bytes_read = 0;
        while bytes_read < length {
            reader.read_exact(&mut header)?;
            bytes_read += 1;

            let h = header[0] as i8;
            if (-127..=-1).contains(&h) {
                let new_len = buffer.len() + (1 - h as isize) as usize;
                reader.read_exact(&mut data)?;
                buffer.resize(new_len, data[0]);
                bytes_read += 1;
            } else if h >= 0 {
                let num_vals = h as usize + 1;
                let start = buffer.len();
                buffer.resize(start + num_vals, 0);
                reader.read_exact(&mut buffer[start..])?;
                bytes_read += num_vals
            } else {
                // h = -128 is a no-op.
            }
        }

        Ok((
            buffer.len(),
            PackBitsReader {
                buffer: io::Cursor::new(buffer),
            },
        ))
    }
}

impl Read for PackBitsReader {
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.buffer.read(buf)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_packbits() {
        let encoded = vec![
            0xFE, 0xAA, 0x02, 0x80, 0x00, 0x2A, 0xFD, 0xAA, 0x03, 0x80, 0x00, 0x2A, 0x22, 0xF7,
            0xAA,
        ];
        let encoded_len = encoded.len();

        let buff = io::Cursor::new(encoded);
        let (_, mut decoder) = PackBitsReader::new(buff, encoded_len).unwrap();

        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();

        let expected = vec![
            0xAA, 0xAA, 0xAA, 0x80, 0x00, 0x2A, 0xAA, 0xAA, 0xAA, 0xAA, 0x80, 0x00, 0x2A, 0x22,
            0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA,
        ];
        assert_eq!(decoded, expected);
    }
}
