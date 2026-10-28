# fwsi

`fwsi` (Flat Whole-Slide Image) is a lightweight archive library for research imaging. It provides a Rust library and a Python package for writing and reading RGB image archives.

It targets a workflow where a WSI level 0 image is read as RGB data, stored in a compact single-file archive, and later accessed through fast random reads of local regions. The project focuses on archive size, region-read performance, and a simple interface that can be used from both Rust and Python.

The project is used through its Rust or Python API and does not provide a command-line interface.

## Python installation

Add the Git dependency to a uv project:

```bash
uv add git+https://github.com/Darcy-Zhang/fwsi-rs.git
```

This updates `pyproject.toml` and `uv.lock`. The relevant entries are:

```toml
[project]
dependencies = [
    "fwsi",
]

[tool.uv.sources]
fwsi = { git = "https://github.com/Darcy-Zhang/fwsi-rs.git" }
```

If `pyproject.toml` is edited manually, use the same entries and then sync the project:

```bash
uv sync
```

Run Python code in the managed environment with:

```bash
uv run python your_script.py
```

The Git installation builds the extension and requires Python 3.10 or newer, Rust stable, Cargo, and a system build toolchain. uv installs Maturin through the build system.

Example:

```python
from pathlib import Path

import fwsi

path = Path("example.fwsi")

writer = fwsi.Writer.create(str(path), 1, 1)
writer.write_tile(0, 0, bytes((255, 0, 0)))
info = writer.finish()

reader = fwsi.open(str(path))
print(reader.info.width, reader.info.height)

region = reader.read(0, 0, 1, 1)
assert (region.width, region.height) == (1, 1)
assert len(region.data) == region.width * region.height * 3
```

## Rust installation

Add the Git dependency to another Cargo project:

```toml
[dependencies]
fwsi = { git = "https://github.com/Darcy-Zhang/fwsi-rs.git" }
```

Example:

```rust
use fwsi::{Reader, Writer};

fn main() -> fwsi::Result<()> {
    let path = "example.fwsi";

    let mut writer = Writer::create(path, 1, 1)?;
    writer.write_tile(0, 0, &[255, 0, 0])?;
    let info = writer.finish()?;
    println!("{}x{}", info.width, info.height);

    let reader = Reader::open(path)?;
    let info = reader.info();
    let region = reader.read(0, 0, 1, 1)?;
    assert_eq!(
        region.data.len(),
        region.width as usize * region.height as usize * 3
    );
    println!("{}x{}", info.width, info.height);

    Ok(())
}
```

## Writing images

Image data uses RGB8 bytes in row-major order, with 3 bytes per pixel. Each archive uses one tile size, which defaults to `1024 x 1024` and can be selected when creating the archive. Write tiles from top to bottom and from left to right:

```text
for tile_y from 0 to tiles_y - 1:
    for tile_x from 0 to tiles_x - 1:
        writer.write_tile(tile_x, tile_y, rgb_bytes)
```

Python can select the archive tile size with `tile_size`:

```python
writer = fwsi.Writer.create("example.fwsi", image_width, image_height, tile_size=256)
```

Rust can select it with:

```rust
let mut writer = fwsi::Writer::create_with_tile_size(
    "example.fwsi",
    image_width,
    image_height,
    256,
)?;
```

The length of each tile buffer must be:

```text
tile_width x tile_height x 3
```

When JPEG tiles are already encoded, convert the complete tile grid directly:

```python
info = fwsi.Writer.create_from_jpeg_tiles(
    "output.fwsi",
    image_width,
    image_height,
    tile_size,
    tiles,
)
```

`tiles[y][x]` contains one JPEG tile as `bytes`. The JPEG bytes are decoded only for the archive checksum and image hash; no external RGB buffer is required. Edge padding is ignored when reading and reads are cropped to the original image dimensions.

The equivalent Rust call is:

```rust
let info = fwsi::Writer::create_from_jpeg_tiles(
    "output.fwsi",
    image_width,
    image_height,
    tile_size,
    &tiles,
)?;
```

The archive keeps the supplied JPEG bytes and does not encode them again. Interior blocks must use the archive tile size. Edge blocks may use any positive dimensions up to their visible dimensions for partial coverage, or the full padded tile dimensions. Pixels outside a partially covered block are returned as zero RGB bytes.

## Reading images

```python
reader = fwsi.open("example.fwsi")
info = reader.info
region = reader.read(x, y, width, height)
rgb = region.data
```

`region.data` contains tightly packed RGB8 bytes with this length:

```text
width x height x 3
```

The equivalent Rust API is:

```rust
let reader = fwsi::Reader::open("example.fwsi")?;
let info = reader.info();
let region = reader.read(x, y, width, height)?;
```

`ImageInfo` provides image dimensions, the tile grid, the image hash, and the content ID. `Region` provides RGB data, width, and height.
