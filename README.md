# fwsi

`fwsi`（Flat Whole-Slide Image）是一个面向科研图像归档的轻量库，提供 Rust 库和 Python 包，用于写入和读取 RGB 图像归档。

它主要服务于这样的工作流：从 WSI 读取 level 0 图像，转换为 RGB 数据，保存为尽可能紧凑的单文件归档，并在后续分析中高速随机读取局部区域。项目关注归档体积、局部读取速度和简单的跨语言使用方式，适合论文研究、图像分析和批处理任务。

项目通过 Rust 或 Python API 使用，不提供命令行入口。

## Python 安装

从 Git 仓库安装：

```bash
python3.13 -m venv .venv
.venv/bin/python -m pip install --upgrade pip
.venv/bin/python -m pip install "git+https://github.com/OWNER/REPOSITORY.git"
```

从本地源码安装：

```bash
python3.13 -m venv .venv
.venv/bin/python -m pip install .
```

从源码安装 Python 包需要 Python 3.13 或更高版本、Rust stable、Cargo 和系统编译工具链。构建系统会自动安装 Maturin。

安装后可以这样使用：

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

## Rust 安装

在另一个 Cargo 项目中添加 Git 依赖：

```toml
[dependencies]
fwsi = { git = "https://github.com/OWNER/REPOSITORY.git" }
```

Rust 使用示例：

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
    assert_eq!(region.data.len(), region.width as usize * region.height as usize * 3);
    println!("{}x{}", info.width, info.height);

    Ok(())
}
```

## 写入图像

图像使用 RGB8 字节，像素按行连续排列，每个像素占 3 个字节。tile 尺寸固定为 `1024 × 1024`；右侧和底部的边缘 tile 使用实际剩余宽高。tile 按从上到下、从左到右的顺序写入：

```text
for tile_y from 0 to tiles_y - 1:
    for tile_x from 0 to tiles_x - 1:
        writer.write_tile(tile_x, tile_y, rgb_bytes)
```

每个 tile 的数据长度必须是：

```text
tile_width × tile_height × 3
```

## 读取图像

```python
reader = fwsi.open("example.fwsi")
info = reader.info
region = reader.read(x, y, width, height)
rgb = region.data
```

`region.data` 是连续排列的 RGB8 字节，长度为：

```text
width × height × 3
```

Rust 对应的接口是：

```rust
let reader = fwsi::Reader::open("example.fwsi")?;
let info = reader.info();
let region = reader.read(x, y, width, height)?;
```

`ImageInfo` 提供图像尺寸、tile 网格、整体 hash 和内容 ID。`Region` 提供 RGB 数据、宽度和高度。
