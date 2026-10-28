use blake3::Hasher;
use rayon::prelude::*;
use rusty_jpeg::decode::{ColorTransform, Decoder, PixelFormat};
use rusty_jpeg::encode::{ColorType, Encoder, SamplingFactor};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
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
const MAX_TILE_SIZE: u32 = u16::MAX as u32;
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
    #[error(
        "tile grid has {actual_rows} rows, expected {expected_rows} with {expected_columns} columns"
    )]
    InvalidTileGrid {
        actual_rows: usize,
        expected_rows: u32,
        expected_columns: u32,
    },
    #[error("tile row {row} has {actual_columns} columns, expected {expected_columns}")]
    InvalidTileRow {
        row: usize,
        actual_columns: usize,
        expected_columns: u32,
    },
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
        if bytes[28] != CHANNELS || bytes[29] != PIXEL_FORMAT_RGB8 || bytes[30] != COMPRESSION_JPEG
        {
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
        if width == 0 || height == 0 || tile_size == 0 || tile_size > MAX_TILE_SIZE {
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
        let expected_index_length = tile_count
            .checked_mul(INDEX_ENTRY_SIZE as u64)
            .ok_or_else(|| Error::InvalidFormat("file layout".to_string()))?;
        if tile_count != tiles_x as u64 * tiles_y as u64 || index_length != expected_index_length {
            return Err(Error::InvalidFormat("tile index dimensions".to_string()));
        }
        let index_end = index_offset
            .checked_add(index_length)
            .ok_or_else(|| Error::InvalidFormat("file layout".to_string()))?;
        if index_offset < HEADER_SIZE as u64 || data_offset < index_end {
            return Err(Error::InvalidFormat("file layout".to_string()));
        }
        Ok(Self {
            info,
            index_offset,
            index_length,
            data_offset,
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

fn valid_tile_dimension(visible: u32, tile_size: u32, actual: u32) -> bool {
    if visible == tile_size {
        actual == tile_size
    } else {
        actual > 0 && (actual <= visible || actual == tile_size)
    }
}

fn valid_tile_dimensions(info: &ImageInfo, x: u32, y: u32, width: u32, height: u32) -> bool {
    let (visible_width, visible_height) = tile_dimensions(info, x, y);
    valid_tile_dimension(visible_width, info.tile_size, width)
        && valid_tile_dimension(visible_height, info.tile_size, height)
}

fn temporary_path(path: &Path) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut temporary = path.as_os_str().to_os_string();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    temporary.push(format!(".fwsi-{}-{}", std::process::id(), stamp));
    PathBuf::from(temporary)
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
    encoder.set_sampling_factor(SamplingFactor::F_1_1);
    encoder.set_optimized_huffman_tables(true);
    encoder
        .encode(raw, width, height, ColorType::Rgb)
        .map_err(|error| Error::Compression(error.to_string()))?;
    Ok(encoded)
}

fn decode_jpeg_data(compressed: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    let mut decoder = Decoder::new(compressed);
    decoder.set_single_threaded(true);
    decoder.set_color_transform(ColorTransform::YCbCr);
    let raw = decoder
        .decode()
        .map_err(|error| Error::Decompression(error.to_string()))?;
    let info = decoder
        .info()
        .ok_or_else(|| Error::InvalidFormat("jpeg image info".to_string()))?;
    if info.pixel_format != PixelFormat::RGB24
        || raw.len() != info.width as usize * info.height as usize * 3
    {
        return Err(Error::InvalidFormat("jpeg tile format".to_string()));
    }
    Ok((raw, info.width as u32, info.height as u32))
}

fn decode_jpeg(compressed: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    if width > MAX_TILE_SIZE || height > MAX_TILE_SIZE {
        return Err(Error::InvalidFormat(
            "tile width or height exceeds JPEG limit".to_string(),
        ));
    }
    let (raw, actual_width, actual_height) = decode_jpeg_data(compressed)?;
    if actual_width != width || actual_height != height {
        return Err(Error::InvalidFormat("jpeg tile dimensions".to_string()));
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
    pub fn create<P: AsRef<Path>>(path: P, width: u64, height: u64) -> Result<Self> {
        Self::create_with_tile_size(path, width, height, DEFAULT_TILE_SIZE)
    }

    pub fn create_from_jpeg_tiles<P: AsRef<Path>>(
        path: P,
        width: u64,
        height: u64,
        tile_size: u32,
        tiles: &[Vec<Vec<u8>>],
    ) -> Result<ImageInfo> {
        let path = path.as_ref();
        let temporary = temporary_path(path);
        let result = (|| {
            let mut writer = Self::create_with_tile_size(&temporary, width, height, tile_size)?;
            writer.write_jpeg_tiles(tiles)?;
            writer.finish()
        })();
        match result {
            Ok(info) => {
                if let Err(error) = fs::rename(&temporary, path) {
                    let _ = fs::remove_file(&temporary);
                    Err(Error::Io(error))
                } else {
                    Ok(info)
                }
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                Err(error)
            }
        }
    }

    pub fn create_with_tile_size<P: AsRef<Path>>(
        path: P,
        width: u64,
        height: u64,
        tile_size: u32,
    ) -> Result<Self> {
        if width == 0 || height == 0 || tile_size == 0 || tile_size > MAX_TILE_SIZE {
            return Err(Error::InvalidFormat("image dimensions".to_string()));
        }
        let tiles_x = u32::try_from(width.div_ceil(tile_size as u64))
            .map_err(|_| Error::InvalidFormat("image dimensions".to_string()))?;
        let tiles_y = u32::try_from(height.div_ceil(tile_size as u64))
            .map_err(|_| Error::InvalidFormat("image dimensions".to_string()))?;
        let tile_count = tiles_x as u64 * tiles_y as u64;
        let index_offset = HEADER_SIZE as u64;
        let index_length = tile_count
            .checked_mul(INDEX_ENTRY_SIZE as u64)
            .ok_or_else(|| Error::InvalidFormat("image dimensions".to_string()))?;
        let data_offset = index_offset
            .checked_add(index_length)
            .ok_or_else(|| Error::InvalidFormat("image dimensions".to_string()))?;
        let tile_count_usize = usize::try_from(tile_count)
            .map_err(|_| Error::InvalidFormat("image dimensions".to_string()))?;
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
        file.set_len(data_offset)?;
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
                    crc32c: 0,
                };
                tile_count_usize
            ],
            data_offset,
            next_tile: 0,
            hasher,
        })
    }

    fn tile_write_info(&self, x: u32, y: u32) -> Result<(u64, u32, u32)> {
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
        Ok((id, width, height))
    }

    fn update_image_hash(
        &mut self,
        x: u32,
        y: u32,
        decoded: &[u8],
        stored_width: u32,
        stored_height: u32,
    ) {
        let (visible_width, visible_height) = tile_dimensions(&self.info, x, y);
        let stored_stride = stored_width as usize * 3;
        let visible_row_length = visible_width as usize * 3;
        let zeros = vec![0u8; visible_row_length];
        for row in 0..visible_height as usize {
            if row < stored_height as usize {
                let available_width = stored_width.min(visible_width) as usize;
                let start = row * stored_stride;
                self.hasher
                    .update(&decoded[start..start + available_width * 3]);
                self.hasher.update(&zeros[available_width * 3..]);
            } else {
                self.hasher.update(&zeros);
            }
        }
    }

    fn write_compressed_tile(
        &mut self,
        x: u32,
        y: u32,
        compressed: &[u8],
        decoded: &[u8],
        stored_width: u32,
        stored_height: u32,
    ) -> Result<()> {
        let id = y as u64 * self.info.tiles_x as u64 + x as u64;
        let offset = self.file.stream_position()?;
        self.file.write_all(compressed)?;
        self.index[id as usize] = TileInfo {
            offset,
            compressed_size: compressed.len() as u64,
            crc32c: crc32c::crc32c(decoded),
        };
        self.update_image_hash(x, y, decoded, stored_width, stored_height);
        self.next_tile += 1;
        Ok(())
    }

    fn write_jpeg_tiles(&mut self, tiles: &[Vec<Vec<u8>>]) -> Result<()> {
        if tiles.len() != self.info.tiles_y as usize {
            return Err(Error::InvalidTileGrid {
                actual_rows: tiles.len(),
                expected_rows: self.info.tiles_y,
                expected_columns: self.info.tiles_x,
            });
        }
        for (y, row) in tiles.iter().enumerate() {
            if row.len() != self.info.tiles_x as usize {
                return Err(Error::InvalidTileRow {
                    row: y,
                    actual_columns: row.len(),
                    expected_columns: self.info.tiles_x,
                });
            }
            for (x, compressed) in row.iter().enumerate() {
                let x = x as u32;
                let y = y as u32;
                self.tile_write_info(x, y)?;
                let (decoded, width, height) = decode_jpeg_data(compressed)?;
                if !valid_tile_dimensions(&self.info, x, y, width, height) {
                    return Err(Error::InvalidFormat("jpeg tile dimensions".to_string()));
                }
                self.write_compressed_tile(x, y, compressed, &decoded, width, height)?;
            }
        }
        Ok(())
    }

    pub fn write_tile(&mut self, x: u32, y: u32, raw: &[u8]) -> Result<()> {
        let (_, width, height) = self.tile_write_info(x, y)?;
        let expected = width as usize * height as usize * 3;
        if raw.len() != expected {
            return Err(Error::InvalidTileLength {
                actual: raw.len(),
                expected,
            });
        }
        let compressed = encode_jpeg(raw, width, height)?;
        let decoded = decode_jpeg(&compressed, width, height)?;
        self.write_compressed_tile(x, y, &compressed, &decoded, width, height)
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
        let header = Header::decode(&header_bytes)?;
        let file_length = file.metadata()?.len();
        let index_end = header
            .index_offset
            .checked_add(header.index_length)
            .ok_or_else(|| Error::InvalidFormat("file layout".to_string()))?;
        if index_end > file_length || header.data_offset > file_length {
            return Err(Error::InvalidFormat("file layout".to_string()));
        }
        let index_length = usize::try_from(header.index_length)
            .map_err(|_| Error::InvalidFormat("index length".to_string()))?;
        let mut index_bytes = vec![0u8; index_length];
        read_at(&file, &mut index_bytes, header.index_offset)?;
        if crc32c::crc32c(&index_bytes) != header.index_crc32c {
            return Err(Error::IndexChecksumMismatch);
        }
        let index = index_bytes
            .as_chunks::<INDEX_ENTRY_SIZE>()
            .0
            .iter()
            .map(|chunk| get_index_entry(chunk))
            .collect::<Vec<_>>();
        let data_length = file_length - header.data_offset;
        if index.iter().any(|entry| {
            entry.offset < header.data_offset
                || entry.offset.saturating_add(entry.compressed_size)
                    > header.data_offset.saturating_add(data_length)
                || entry.compressed_size == 0
        }) {
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
        Ok((y as u64 * self.inner.header.info.tiles_x as u64 + x as u64) as usize)
    }

    fn decode_tile_bytes(
        &self,
        x: u32,
        y: u32,
        compressed: &[u8],
        entry: TileInfo,
    ) -> Result<(Vec<u8>, u32, u32)> {
        let (raw, width, height) = decode_jpeg_data(compressed)?;
        if !valid_tile_dimensions(&self.inner.header.info, x, y, width, height) {
            return Err(Error::InvalidFormat("jpeg tile dimensions".to_string()));
        }
        if crc32c::crc32c(&raw) != entry.crc32c {
            return Err(Error::ChecksumMismatch);
        }
        Ok((raw, width, height))
    }

    pub fn read(&self, x: u64, y: u64, width: u32, height: u32) -> Result<Region> {
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
                    let (data, width, height) =
                        self.decode_tile_bytes(job.0, job.1, &block[start..end], job.3)?;
                    Ok::<_, Error>((job.0, job.1, data, width, height))
                })
                .collect::<Vec<_>>();
            for part in parts {
                decoded.push(part?);
            }
        }
        let stride = width as usize * 3;
        let mut output = vec![0u8; stride * height as usize];
        for (tx, ty, tile, stored_w, stored_h) in decoded {
            let (tile_w, tile_h) = tile_dimensions(&self.inner.header.info, tx, ty);
            let src_stride = stored_w as usize * 3;
            let tile_origin_x = tx as u64 * self.inner.header.info.tile_size as u64;
            let tile_origin_y = ty as u64 * self.inner.header.info.tile_size as u64;
            let copy_x = tile_origin_x.saturating_sub(x) as usize;
            let copy_y = tile_origin_y.saturating_sub(y) as usize;
            let src_start_x = x.saturating_sub(tile_origin_x) as usize;
            let src_start_y = y.saturating_sub(tile_origin_y) as usize;
            let visible_w = tile_w
                .saturating_sub(src_start_x as u32)
                .min(width.saturating_sub(copy_x as u32))
                .min(stored_w.saturating_sub(src_start_x as u32));
            let visible_h = tile_h
                .saturating_sub(src_start_y as u32)
                .min(height.saturating_sub(copy_y as u32))
                .min(stored_h.saturating_sub(src_start_y as u32));
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

#[cfg(test)]
mod tests {
    use super::{Error, HEADER_SIZE, Header, Reader, Writer, decode_jpeg, encode_jpeg};
    use std::fs;

    #[test]
    fn jpeg_rgb_round_trip() {
        for expected in [[255u8, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]] {
            let mut input = Vec::with_capacity(32 * 32 * 3);
            for _ in 0..(32 * 32) {
                input.extend_from_slice(&expected);
            }
            let compressed = encode_jpeg(&input, 32, 32).unwrap();
            let decoded = decode_jpeg(&compressed, 32, 32).unwrap();
            for (actual, expected) in decoded[..3].iter().zip(expected) {
                assert!((*actual as i16 - expected as i16).abs() <= 5);
            }
        }
    }

    #[test]
    fn jpeg_tiles_conversion_preserves_compressed_bytes() {
        let path = std::env::temp_dir().join(format!("fwsi-jpeg-tiles-{}", std::process::id()));
        let raw = [255u8, 0, 0].repeat(32 * 32);
        let compressed = encode_jpeg(&raw, 32, 32).unwrap();
        let info = Writer::create_from_jpeg_tiles(&path, 32, 32, 1024, &[vec![compressed.clone()]])
            .unwrap();
        let archive = fs::read(&path).unwrap();
        let header = Header::decode(&archive[..HEADER_SIZE].try_into().unwrap()).unwrap();
        assert_eq!(
            &archive[header.data_offset as usize..],
            compressed.as_slice()
        );

        let reader = Reader::open(&path).unwrap();
        assert_eq!(reader.info().image_hash, info.image_hash);
        assert_eq!(
            reader.read(0, 0, 32, 32).unwrap().data,
            decode_jpeg(&compressed, 32, 32).unwrap()
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn padded_edge_jpeg_tiles_are_cropped() {
        let path = std::env::temp_dir().join(format!("fwsi-padded-edge-{}", std::process::id()));
        let red = encode_jpeg(&[255u8, 0, 0].repeat(256 * 256), 256, 256).unwrap();
        let green = encode_jpeg(&[0u8, 255, 0].repeat(256 * 256), 256, 256).unwrap();
        let red_rgb = decode_jpeg(&red, 256, 256).unwrap();
        let green_rgb = decode_jpeg(&green, 256, 256).unwrap();
        Writer::create_from_jpeg_tiles(&path, 300, 250, 256, &[vec![red, green]]).unwrap();

        let reader = Reader::open(&path).unwrap();
        let region = reader.read(255, 249, 2, 1).unwrap();
        let red_start = (249 * 256 + 255) * 3;
        let green_start = (249 * 256) * 3;
        let mut expected = red_rgb[red_start..red_start + 3].to_vec();
        expected.extend_from_slice(&green_rgb[green_start..green_start + 3]);
        assert_eq!(region.data, expected);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn unpadded_edge_jpeg_tiles_are_cropped() {
        let path = std::env::temp_dir().join(format!("fwsi-unpadded-edge-{}", std::process::id()));
        let red = encode_jpeg(&[255u8, 0, 0].repeat(256 * 250), 256, 250).unwrap();
        let green = encode_jpeg(&[0u8, 255, 0].repeat(44 * 250), 44, 250).unwrap();
        let red_rgb = decode_jpeg(&red, 256, 250).unwrap();
        let green_rgb = decode_jpeg(&green, 44, 250).unwrap();
        Writer::create_from_jpeg_tiles(&path, 300, 250, 256, &[vec![red, green]]).unwrap();

        let reader = Reader::open(&path).unwrap();
        let region = reader.read(255, 249, 2, 1).unwrap();
        let red_start = (249 * 256 + 255) * 3;
        let green_start = 249 * 44 * 3;
        let mut expected = red_rgb[red_start..red_start + 3].to_vec();
        expected.extend_from_slice(&green_rgb[green_start..green_start + 3]);
        assert_eq!(region.data, expected);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn partially_covered_edge_jpeg_tiles_are_blank_after_coverage() {
        let path = std::env::temp_dir().join(format!("fwsi-partial-edge-{}", std::process::id()));
        let red = encode_jpeg(&[255u8, 0, 0].repeat(256 * 13), 256, 13).unwrap();
        let green = encode_jpeg(&[0u8, 255, 0].repeat(44 * 13), 44, 13).unwrap();
        let red_rgb = decode_jpeg(&red, 256, 13).unwrap();
        let green_rgb = decode_jpeg(&green, 44, 13).unwrap();
        Writer::create_from_jpeg_tiles(&path, 300, 250, 256, &[vec![red, green]]).unwrap();

        let reader = Reader::open(&path).unwrap();
        let region = reader.read(255, 12, 2, 2).unwrap();
        let red_start = (12 * 256 + 255) * 3;
        let green_start = 12 * 44 * 3;
        let mut expected = red_rgb[red_start..red_start + 3].to_vec();
        expected.extend_from_slice(&green_rgb[green_start..green_start + 3]);
        expected.extend_from_slice(&[0u8; 6]);
        assert_eq!(region.data, expected);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn partially_covered_interior_jpeg_tiles_are_rejected() {
        let path =
            std::env::temp_dir().join(format!("fwsi-partial-interior-{}", std::process::id()));
        let partial = encode_jpeg(&[255u8, 0, 0].repeat(256 * 13), 256, 13).unwrap();
        let full = encode_jpeg(&[0u8, 255, 0].repeat(256 * 256), 256, 256).unwrap();
        let result = Writer::create_from_jpeg_tiles(&path, 512, 256, 256, &[vec![partial, full]]);
        assert!(matches!(result, Err(Error::InvalidFormat(_))));
        assert!(!path.exists());
    }
}

#[cfg(feature = "python")]
pub mod python;

#[cfg(feature = "python")]
pub use python::fwsi;
