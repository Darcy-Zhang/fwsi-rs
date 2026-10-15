use blake3::Hasher;
use rayon::prelude::*;
use rusty_jpeg::decode::{ColorTransform, Decoder, PixelFormat};
use rusty_jpeg::encode::{ColorType, Encoder, SamplingFactor};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;
use thiserror::Error;

pub const MAGIC: [u8; 4] = *b"FWSI";
pub const VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 128;
pub const INDEX_ENTRY_SIZE: usize = 20;
pub const DEFAULT_TILE_SIZE: u32 = 1024;

const CHANNELS: u8 = 3;
const PIXEL_FORMAT_RGB8: u8 = 1;
const COMPRESSION_JPEG: u8 = 2;
const JPEG_QUALITY: u8 = 80;
const MAX_JPEG_DIMENSION: u32 = u16::MAX as u32;
const MAX_MERGED_READ: u64 = 8 * 1024 * 1024;
const MAX_MERGED_GAP: u64 = 256 * 1024;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid FWSI file: {0}")]
    InvalidFormat(String),
    #[error("tile coordinate is outside the image")]
    TileOutOfBounds,
    #[error("region is outside the image")]
    RegionOutOfBounds,
    #[error("tile data has length {actual}, expected {expected}")]
    InvalidTileLength { actual: usize, expected: usize },
    #[error("tile write order is {actual}, expected {expected}")]
    InvalidTileOrder { actual: u64, expected: u64 },
    #[error("tile checksum mismatch")]
    ChecksumMismatch,
    #[error("header checksum mismatch")]
    HeaderChecksumMismatch,
    #[error("index checksum mismatch")]
    IndexChecksumMismatch,
    #[error("image is incomplete")]
    IncompleteImage,
    #[error("decompression error: {0}")]
    Decompression(String),
    #[error("compression error: {0}")]
    Compression(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileInfo {
    pub offset: u64,
    pub compressed_size: u64,
    pub crc32c: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    pub width: u64,
    pub height: u64,
    pub tile_size: u32,
    pub tiles_x: u32,
    pub tiles_y: u32,
    pub tile_count: u64,
    pub image_hash: [u8; 32],
}

impl ImageInfo {
    pub fn content_id(&self) -> String {
        base32_lower(&self.image_hash[..16])
    }

    pub fn image_hash_hex(&self) -> String {
        hex_lower(&self.image_hash)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy)]
struct Header {
    info: ImageInfo,
    index_offset: u64,
    index_length: u64,
    data_offset: u64,
    data_length: u64,
    index_crc32c: u32,
}

impl Header {
    fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut out = [0u8; HEADER_SIZE];
        out[0..4].copy_from_slice(&MAGIC);
        out[4..6].copy_from_slice(&VERSION.to_le_bytes());
        out[6..8].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
        out[8..16].copy_from_slice(&self.info.width.to_le_bytes());
        out[16..24].copy_from_slice(&self.info.height.to_le_bytes());
        out[24..28].copy_from_slice(&self.info.tile_size.to_le_bytes());
        out[28] = CHANNELS;
        out[29] = PIXEL_FORMAT_RGB8;
        out[30] = COMPRESSION_JPEG;
        out[32..36].copy_from_slice(&self.info.tiles_x.to_le_bytes());
        out[36..40].copy_from_slice(&self.info.tiles_y.to_le_bytes());
        out[40..48].copy_from_slice(&self.info.tile_count.to_le_bytes());
        out[48..56].copy_from_slice(&self.index_offset.to_le_bytes());
        out[56..60].copy_from_slice(&(INDEX_ENTRY_SIZE as u32).to_le_bytes());
        out[64..72].copy_from_slice(&self.index_length.to_le_bytes());
        out[72..80].copy_from_slice(&self.data_offset.to_le_bytes());
        out[80..112].copy_from_slice(&self.info.image_hash);
        out[112..116].copy_from_slice(&self.index_crc32c.to_le_bytes());
        let crc = crc32c::crc32c(&out[..116]);
        out[116..120].copy_from_slice(&crc.to_le_bytes());
        out
    }

    fn decode(bytes: &[u8; HEADER_SIZE]) -> Result<Self> {
        if bytes[0..4] != MAGIC {
            return Err(Error::InvalidFormat("magic".to_string()));
        }
        if u16::from_le_bytes(bytes[4..6].try_into().unwrap()) != VERSION {
            return Err(Error::InvalidFormat("version".to_string()));
        }
        if u16::from_le_bytes(bytes[6..8].try_into().unwrap()) as usize != HEADER_SIZE {
            return Err(Error::InvalidFormat("header size".to_string()));
        }
        let expected_crc = u32::from_le_bytes(bytes[116..120].try_into().unwrap());
        if crc32c::crc32c(&bytes[..116]) != expected_crc {
            return Err(Error::HeaderChecksumMismatch);
        }
        let width = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        let height = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
        let tile_size = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        if bytes[28] != CHANNELS || bytes[29] != PIXEL_FORMAT_RGB8 || bytes[30] != COMPRESSION_JPEG {
            return Err(Error::InvalidFormat("encoding".to_string()));
        }
        let tiles_x = u32::from_le_bytes(bytes[32..36].try_into().unwrap());
        let tiles_y = u32::from_le_bytes(bytes[36..40].try_into().unwrap());
        let tile_count = u64::from_le_bytes(bytes[40..48].try_into().unwrap());
        let index_offset = u64::from_le_bytes(bytes[48..56].try_into().unwrap());
        let index_entry_size = u32::from_le_bytes(bytes[56..60].try_into().unwrap());
        if index_entry_size as usize != INDEX_ENTRY_SIZE {
            return Err(Error::InvalidFormat("index entry size".to_string()));
        }
        let index_length = u64::from_le_bytes(bytes[64..72].try_into().unwrap());
        let data_offset = u64::from_le_bytes(bytes[72..80].try_into().unwrap());
        let index_crc32c = u32::from_le_bytes(bytes[112..116].try_into().unwrap());
        let image_hash = bytes[80..112].try_into().unwrap();
        if width == 0 || height == 0 || tile_size == 0 || tile_size > MAX_JPEG_DIMENSION {
            return Err(Error::InvalidFormat("tile index dimensions".to_string()));
        }
        let expected_tiles_x = width.div_ceil(tile_size as u64);
        let expected_tiles_y = height.div_ceil(tile_size as u64);
        if expected_tiles_x > u32::MAX as u64
            || expected_tiles_y > u32::MAX as u64
            || tiles_x as u64 != expected_tiles_x
            || tiles_y as u64 != expected_tiles_y
        {
            return Err(Error::InvalidFormat("tile index dimensions".to_string()));
        }
        let info = ImageInfo {
            width,
            height,
            tile_size,
            tiles_x,
            tiles_y,
            tile_count,
            image_hash,
        };
        if tile_count != tiles_x as u64 * tiles_y as u64
            || index_length != tile_count * INDEX_ENTRY_SIZE as u64
        {
            return Err(Error::InvalidFormat("tile index dimensions".to_string()));
        }
        if index_offset < HEADER_SIZE as u64 || data_offset < index_offset + index_length {
            return Err(Error::InvalidFormat("file layout".to_string()));
        }
        Ok(Self {
            info,
            index_offset,
            index_length,
            data_offset,
            data_length: 0,
            index_crc32c,
        })
    }
}

fn tile_dimensions(info: &ImageInfo, x: u32, y: u32) -> (u32, u32) {
    let px = x as u64 * info.tile_size as u64;
    let py = y as u64 * info.tile_size as u64;
    let w = (info.width - px).min(info.tile_size as u64) as u32;
    let h = (info.height - py).min(info.tile_size as u64) as u32;
    (w, h)
}

fn update_hash_context(hasher: &mut Hasher, info: &ImageInfo) {
    hasher.update(b"FWSI\x01JPEG-RGB8-444\x00");
    hasher.update(&info.width.to_le_bytes());
    hasher.update(&info.height.to_le_bytes());
    hasher.update(&info.tile_size.to_le_bytes());
}

fn put_index_entry(out: &mut [u8], entry: TileInfo) {
    out[0..8].copy_from_slice(&entry.offset.to_le_bytes());
    out[8..16].copy_from_slice(&entry.compressed_size.to_le_bytes());
    out[16..20].copy_from_slice(&entry.crc32c.to_le_bytes());
}

fn get_index_entry(bytes: &[u8]) -> TileInfo {
    TileInfo {
        offset: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
        compressed_size: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        crc32c: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
    }
}

fn encode_jpeg(raw: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let width = u16::try_from(width)
        .map_err(|_| Error::Compression("tile width exceeds JPEG limit".to_string()))?;
    let height = u16::try_from(height)
        .map_err(|_| Error::Compression("tile height exceeds JPEG limit".to_string()))?;
    let mut encoded = Vec::new();
    let mut encoder = Encoder::new(&mut encoded, JPEG_QUALITY);
    // Pathology RGB tiles contain meaningful stain/color boundaries. Keep the
    // chroma plane at full resolution instead of the encoder's default 4:2:0.
    encoder.set_sampling_factor(SamplingFactor::F_1_1);
    // This is an offline archive writer, so spend a little more CPU to avoid
    // leaving easy Huffman coding gains on the table.
    encoder.set_optimized_huffman_tables(true);
    encoder
        .encode(raw, width, height, ColorType::Rgb)
        .map_err(|error| Error::Compression(error.to_string()))?;
    Ok(encoded)
}

fn decode_jpeg(compressed: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let mut decoder = Decoder::new(compressed);
    // Region reads already parallelize at the tile level with Rayon. Avoid
    // creating another worker pool inside every JPEG decode.
    decoder.set_single_threaded(true);
    decoder.set_color_transform(ColorTransform::RGB);
    let raw = decoder
        .decode()
        .map_err(|error| Error::Decompression(error.to_string()))?;
    let expected_width = u16::try_from(width)
        .map_err(|_| Error::InvalidFormat("tile width exceeds JPEG limit".to_string()))?;
    let expected_height = u16::try_from(height)
        .map_err(|_| Error::InvalidFormat("tile height exceeds JPEG limit".to_string()))?;
    let expected_len = width as usize * height as usize * 3;
    let info = decoder
        .info()
        .ok_or_else(|| Error::InvalidFormat("jpeg image info".to_string()))?;
    if info.width != expected_width
        || info.height != expected_height
        || info.pixel_format != PixelFormat::RGB24
        || raw.len() != expected_len
    {
        return Err(Error::InvalidFormat(
            "jpeg tile dimensions or format".to_string(),
        ));
    }
    Ok(raw)
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = file.read_at(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected end of file",
            ));
        }
        done += n;
    }
    Ok(())
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected end of file",
            ));
        }
        done += n;
    }
    Ok(())
}

pub struct Writer {
    file: File,
    info: ImageInfo,
    index: Vec<TileInfo>,
    data_offset: u64,
    next_tile: u64,
    hasher: Hasher,
}

impl Writer {
    pub fn create<P: AsRef<Path>>(
        path: P,
        width: u64,
        height: u64,
        tile_size: u32,
    ) -> Result<Self> {
        if width == 0 || height == 0 || tile_size == 0 || tile_size > MAX_JPEG_DIMENSION {
            return Err(Error::InvalidFormat("image dimensions".to_string()));
        }
        let tiles_x = u32::try_from(width.div_ceil(tile_size as u64))
            .map_err(|_| Error::InvalidFormat("image dimensions".to_string()))?;
        let tiles_y = u32::try_from(height.div_ceil(tile_size as u64))
            .map_err(|_| Error::InvalidFormat("image dimensions".to_string()))?;
        let tile_count = tiles_x as u64 * tiles_y as u64;
        let index_offset = HEADER_SIZE as u64;
        let index_length = tile_count * INDEX_ENTRY_SIZE as u64;
        let data_offset = index_offset + index_length;
        let info = ImageInfo {
            width,
            height,
            tile_size,
            tiles_x,
            tiles_y,
            tile_count,
            image_hash: [0; 32],
        };
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path.as_ref())?;
        file.write_all(&[0u8; HEADER_SIZE])?;
        file.write_all(&vec![0u8; index_length as usize])?;
        file.seek(SeekFrom::Start(data_offset))?;
        let mut hasher = Hasher::new();
        update_hash_context(&mut hasher, &info);
        Ok(Self {
            file,
            info,
            index: vec![
                TileInfo {
                    offset: 0,
                    compressed_size: 0,
                    raw_size: 0,
                    crc32c: 0
                };
                tile_count as usize
            ],
            data_offset,
            next_tile: 0,
            hasher,
        })
    }

    pub fn write_tile(&mut self, x: u32, y: u32, raw: &[u8]) -> Result<()> {
        if x >= self.info.tiles_x || y >= self.info.tiles_y {
            return Err(Error::TileOutOfBounds);
        }
        let id = y as u64 * self.info.tiles_x as u64 + x as u64;
        if id != self.next_tile {
            return Err(Error::InvalidTileOrder {
                actual: id,
                expected: self.next_tile,
            });
        }
        let (width, height) = tile_dimensions(&self.info, x, y);
        let expected = width as usize * height as usize * 3;
        if raw.len() != expected {
            return Err(Error::InvalidTileLength {
                actual: raw.len(),
                expected,
            });
        }
        let compressed = encode_jpeg(raw, width, height)?;
        // JPEG is lossy, so the canonical image stream is the RGB data that a
        // reader receives after decoding this tile, not the pre-encode input.
        let decoded = decode_jpeg(&compressed, width, height)?;
        let offset = self.file.stream_position()?;
        self.file.write_all(&compressed)?;
        self.index[id as usize] = TileInfo {
            offset,
            compressed_size: compressed.len() as u64,
            crc32c: crc32c::crc32c(&decoded),
        };
        self.hasher.update(&decoded);
        self.next_tile += 1;
        Ok(())
    }

    pub fn finish(mut self) -> Result<ImageInfo> {
        if self.next_tile != self.info.tile_count {
            return Err(Error::IncompleteImage);
        }
        let hash = *self.hasher.finalize().as_bytes();
        self.info.image_hash = hash;
        let mut index_bytes = vec![0u8; self.index.len() * INDEX_ENTRY_SIZE];
        for (i, entry) in self.index.iter().copied().enumerate() {
            put_index_entry(
                &mut index_bytes[i * INDEX_ENTRY_SIZE..][..INDEX_ENTRY_SIZE],
                entry,
            );
        }
        let index_crc32c = crc32c::crc32c(&index_bytes);
        let header = Header {
            info: self.info,
            index_offset: HEADER_SIZE as u64,
            index_length: index_bytes.len() as u64,
            data_offset: self.data_offset,
            data_length: self.file.stream_position()? - self.data_offset,
            index_crc32c,
        };
        self.file.seek(SeekFrom::Start(header.index_offset))?;
        self.file.write_all(&index_bytes)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header.encode())?;
        self.file.sync_all()?;
        Ok(self.info)
    }

}

struct ReaderInner {
    file: File,
    header: Header,
    index: Vec<TileInfo>,
}

#[derive(Clone)]
pub struct Reader {
    inner: Arc<ReaderInner>,
}

impl Reader {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut file = File::open(path)?;
        let mut header_bytes = [0u8; HEADER_SIZE];
        file.read_exact(&mut header_bytes)?;
        let mut header = Header::decode(&header_bytes)?;
        let mut index_bytes = vec![0u8; header.index_length as usize];
        file.read_exact_at_offset(&mut index_bytes, header.index_offset)?;
        if crc32c::crc32c(&index_bytes) != header.index_crc32c {
            return Err(Error::IndexChecksumMismatch);
        }
        let index = index_bytes
            .as_chunks::<INDEX_ENTRY_SIZE>()
            .0
            .iter()
            .map(|chunk| get_index_entry(chunk))
            .collect::<Vec<_>>();
        let data_length = file.metadata()?.len().saturating_sub(header.data_offset);
        header.data_length = data_length;
        if header.lenient_invalid_entries(&index, data_length) {
            return Err(Error::InvalidFormat("tile index bounds".to_string()));
        }
        Ok(Self {
            inner: Arc::new(ReaderInner {
                file,
                header,
                index,
            }),
        })
    }

    pub fn info(&self) -> ImageInfo {
        self.inner.header.info
    }

    fn tile_id(&self, x: u32, y: u32) -> Result<usize> {
        if x >= self.inner.header.info.tiles_x || y >= self.inner.header.info.tiles_y {
            return Err(Error::TileOutOfBounds);
        }
        Ok((y * self.inner.header.info.tiles_x + x) as usize)
    }

    fn read_compressed(&self, entry: TileInfo) -> Result<Vec<u8>> {
        let mut bytes = vec![0u8; entry.compressed_size as usize];
        read_at(&self.inner.file, &mut bytes, entry.offset)?;
        Ok(bytes)
    }

    fn decode_tile_bytes(
        &self,
        x: u32,
        y: u32,
        compressed: &[u8],
        entry: TileInfo,
    ) -> Result<Vec<u8>> {
        let (width, height) = tile_dimensions(&self.inner.header.info, x, y);
        let raw = decode_jpeg(compressed, width, height)?;
        if crc32c::crc32c(&raw) != entry.crc32c {
            return Err(Error::ChecksumMismatch);
        }
        Ok(raw)
    }

    pub fn read_tile(&self, x: u32, y: u32) -> Result<Vec<u8>> {
        let id = self.tile_id(x, y)?;
        let entry = self.inner.index[id];
        let compressed = self.read_compressed(entry)?;
        self.decode_tile_bytes(x, y, &compressed, entry)
    }

    pub fn read_region(&self, x: u64, y: u64, width: u32, height: u32) -> Result<Region> {
        if width == 0
            || height == 0
            || x.checked_add(width as u64).is_none()
            || y.checked_add(height as u64).is_none()
            || x + width as u64 > self.inner.header.info.width
            || y + height as u64 > self.inner.header.info.height
        {
            return Err(Error::RegionOutOfBounds);
        }
        let first_x = (x / self.inner.header.info.tile_size as u64) as u32;
        let first_y = (y / self.inner.header.info.tile_size as u64) as u32;
        let last_x = ((x + width as u64 - 1) / self.inner.header.info.tile_size as u64) as u32;
        let last_y = ((y + height as u64 - 1) / self.inner.header.info.tile_size as u64) as u32;
        let mut jobs = Vec::new();
        for ty in first_y..=last_y {
            for tx in first_x..=last_x {
                let id = self.tile_id(tx, ty)?;
                jobs.push((tx, ty, id, self.inner.index[id]));
            }
        }
        jobs.sort_by_key(|(_, _, _, entry)| entry.offset);
        let batches = merge_jobs(&jobs);
        let mut decoded = Vec::with_capacity(jobs.len());
        for batch in batches {
            let mut block = vec![0u8; (batch.end - batch.start) as usize];
            read_at(&self.inner.file, &mut block, batch.start)?;
            let parts = batch
                .jobs
                .par_iter()
                .map(|job| {
                    let start = (job.3.offset - batch.start) as usize;
                    let end = start + job.3.compressed_size as usize;
                    let data = self.decode_tile_bytes(job.0, job.1, &block[start..end], job.3)?;
                    Ok::<_, Error>((job.0, job.1, data))
                })
                .collect::<Vec<_>>();
            for part in parts {
                decoded.push(part?);
            }
        }
        let stride = width as usize * 3;
        let mut output = vec![0u8; stride * height as usize];
        for (tx, ty, tile) in decoded {
            let (tile_w, tile_h) = tile_dimensions(&self.inner.header.info, tx, ty);
            let src_stride = tile_w as usize * 3;
            let tile_origin_x = tx as u64 * self.inner.header.info.tile_size as u64;
            let tile_origin_y = ty as u64 * self.inner.header.info.tile_size as u64;
            let copy_x = tile_origin_x.saturating_sub(x) as usize;
            let copy_y = tile_origin_y.saturating_sub(y) as usize;
            let src_start_x = x.saturating_sub(tile_origin_x) as usize;
            let src_start_y = y.saturating_sub(tile_origin_y) as usize;
            let visible_w = tile_w
                .saturating_sub(src_start_x as u32)
                .min(width.saturating_sub(copy_x as u32));
            let visible_h = tile_h
                .saturating_sub(src_start_y as u32)
                .min(height.saturating_sub(copy_y as u32));
            for row in 0..visible_h as usize {
                let src = (src_start_y + row) * src_stride + src_start_x * 3;
                let dst = (copy_y + row) * stride + copy_x * 3;
                output[dst..dst + visible_w as usize * 3]
                    .copy_from_slice(&tile[src..src + visible_w as usize * 3]);
            }
        }
        Ok(Region {
            data: output,
            width,
            height,
        })
    }

    pub fn verify(&self) -> Result<ImageInfo> {
        let mut hasher = Hasher::new();
        update_hash_context(&mut hasher, &self.inner.header.info);
        for id in 0..self.inner.header.info.tile_count {
            let x = (id as u32) % self.inner.header.info.tiles_x;
            let y = (id as u32) / self.inner.header.info.tiles_x;
            hasher.update(&self.read_tile(x, y)?);
        }
        let actual = *hasher.finalize().as_bytes();
        if actual != self.inner.header.info.image_hash {
            return Err(Error::InvalidFormat("image hash mismatch".to_string()));
        }
        Ok(self.info())
    }
}

trait ReadAtOffset {
    fn read_exact_at_offset(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
}

impl ReadAtOffset for File {
    fn read_exact_at_offset(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        read_at(self, buf, offset)
    }
}

struct ReadBatch<'a> {
    start: u64,
    end: u64,
    jobs: Vec<&'a (u32, u32, usize, TileInfo)>,
}

fn merge_jobs<'a>(jobs: &'a [(u32, u32, usize, TileInfo)]) -> Vec<ReadBatch<'a>> {
    let mut batches: Vec<ReadBatch<'a>> = Vec::new();
    for job in jobs {
        let start = job.3.offset;
        let end = start + job.3.compressed_size;
        if let Some(last) = batches.last_mut() {
            let gap = start.saturating_sub(last.end);
            if gap <= MAX_MERGED_GAP && end - last.start <= MAX_MERGED_READ {
                last.end = last.end.max(end);
                last.jobs.push(job);
                continue;
            }
        }
        batches.push(ReadBatch {
            start,
            end,
            jobs: vec![job],
        });
    }
    batches
}

trait IndexBounds {
    fn lenient_invalid_entries(&self, index: &[TileInfo], data_length: u64) -> bool;
}

impl IndexBounds for Header {
    fn lenient_invalid_entries(&self, index: &[TileInfo], data_length: u64) -> bool {
        index.iter().any(|entry| {
            entry.offset < self.data_offset
                || entry.offset.saturating_add(entry.compressed_size)
                    > self.data_offset.saturating_add(data_length)
                || entry.raw_size == 0
        })
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(TABLE[(byte >> 4) as usize] as char);
        out.push(TABLE[(byte & 15) as usize] as char);
    }
    out
}

fn base32_lower(bytes: &[u8]) -> String {
    const TABLE: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::new();
    let mut buffer = 0u16;
    let mut bits = 0u8;
    for byte in bytes {
        buffer = (buffer << 8) | *byte as u16;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(TABLE[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(TABLE[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

#[cfg(feature = "python")]
pub mod python_api {
    use super::*;
    use pyo3::exceptions::PyValueError;
    use pyo3::prelude::*;
    use pyo3::types::PyBytes;
    use pyo3_stub_gen::{
        define_stub_info_gatherer,
        derive::{gen_stub_pyclass, gen_stub_pyfunction, gen_stub_pymethods},
    };

    fn py_err(error: Error) -> PyErr {
        PyValueError::new_err(error.to_string())
    }

    #[gen_stub_pyclass]
    #[pyclass(name = "ImageInfo", frozen)]
    #[derive(Clone)]
    pub struct PyImageInfo {
        inner: ImageInfo,
    }

    #[gen_stub_pymethods]
    #[pymethods]
    impl PyImageInfo {
        #[getter]
        fn width(&self) -> u64 {
            self.inner.width
        }
        #[getter]
        fn height(&self) -> u64 {
            self.inner.height
        }
        #[getter]
        fn tile_size(&self) -> u32 {
            self.inner.tile_size
        }
        #[getter]
        fn tiles_x(&self) -> u32 {
            self.inner.tiles_x
        }
        #[getter]
        fn tiles_y(&self) -> u32 {
            self.inner.tiles_y
        }
        #[getter]
        fn tile_count(&self) -> u64 {
            self.inner.tile_count
        }
        #[getter]
        fn image_hash<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
            PyBytes::new(py, &self.inner.image_hash)
        }
        #[getter]
        fn image_hash_hex(&self) -> String {
            self.inner.image_hash_hex()
        }
        #[getter]
        fn content_id(&self) -> String {
            self.inner.content_id()
        }
    }

    #[gen_stub_pyclass]
    #[pyclass(name = "Region", frozen)]
    pub struct PyRegion {
        inner: Region,
    }

    #[gen_stub_pymethods]
    #[pymethods]
    impl PyRegion {
        #[getter]
        fn data<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
            PyBytes::new(py, &self.inner.data)
        }
        #[getter]
        fn width(&self) -> u32 {
            self.inner.width
        }
        #[getter]
        fn height(&self) -> u32 {
            self.inner.height
        }
    }

    #[gen_stub_pyclass]
    #[pyclass(name = "Writer")]
    pub struct PyWriter {
        inner: Option<Writer>,
    }

    #[gen_stub_pymethods]
    #[pymethods]
    impl PyWriter {
        #[staticmethod]
        #[pyo3(signature = (path, width, height, tile_size=1024))]
        fn create(
            path: String,
            width: u64,
            height: u64,
            tile_size: u32,
        ) -> PyResult<Self> {
            Ok(Self {
                inner: Some(
                    Writer::create(path, width, height, tile_size).map_err(py_err)?,
                ),
            })
        }
        fn write_tile(
            &mut self,
            py: Python<'_>,
            x: u32,
            y: u32,
            data: &Bound<'_, PyBytes>,
        ) -> PyResult<()> {
            let bytes = data.as_bytes().to_vec();
            let mut writer = self.inner.take().unwrap();
            let (writer, result) = py.detach(|| {
                let result = writer.write_tile(x, y, &bytes);
                (writer, result)
            });
            self.inner = Some(writer);
            result.map_err(py_err)
        }
        fn finish(&mut self, py: Python<'_>) -> PyResult<PyImageInfo> {
            let writer = self.inner.take().unwrap();
            Ok(PyImageInfo {
                inner: py.detach(|| writer.finish().map_err(py_err))?,
            })
        }
    }

    #[gen_stub_pyclass]
    #[pyclass(name = "Reader", frozen)]
    pub struct PyReader {
        inner: Reader,
    }

    #[gen_stub_pymethods]
    #[pymethods]
    impl PyReader {
        #[staticmethod]
        fn open(path: String) -> PyResult<Self> {
            Ok(Self {
                inner: Reader::open(path).map_err(py_err)?,
            })
        }
        #[getter]
        fn info(&self) -> PyImageInfo {
            PyImageInfo {
                inner: self.inner.info(),
            }
        }
        fn read_tile<'py>(&self, py: Python<'py>, x: u32, y: u32) -> PyResult<Bound<'py, PyBytes>> {
            let inner = self.inner.clone();
            let data = py.detach(|| inner.read_tile(x, y).map_err(py_err))?;
            Ok(PyBytes::new(py, &data))
        }
        #[pyo3(signature = (x, y, width, height))]
        fn read_region(
            &self,
            py: Python<'_>,
            x: u64,
            y: u64,
            width: u32,
            height: u32,
        ) -> PyResult<PyRegion> {
            let inner = self.inner.clone();
            py.detach(|| {
                inner
                    .read_region(x, y, width, height)
                    .map(|region| PyRegion { inner: region })
                    .map_err(py_err)
            })
        }
        fn verify(&self, py: Python<'_>) -> PyResult<PyImageInfo> {
            let inner = self.inner.clone();
            py.detach(|| {
                inner
                    .verify()
                    .map(|info| PyImageInfo { inner: info })
                    .map_err(py_err)
            })
        }
    }

    #[gen_stub_pyfunction]
    #[pyfunction]
    fn open(path: String) -> PyResult<PyReader> {
        PyReader::open(path)
    }

    #[pymodule]
    pub fn fwsi(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add_class::<PyImageInfo>()?;
        m.add_class::<PyRegion>()?;
        m.add_class::<PyWriter>()?;
        m.add_class::<PyReader>()?;
        m.add_function(wrap_pyfunction!(open, m)?)?;
        Ok(())
    }

    define_stub_info_gatherer!(stub_info);
}

#[cfg(feature = "python")]
pub use python_api::fwsi;
