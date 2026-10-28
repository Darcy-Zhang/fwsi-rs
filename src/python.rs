use super::*;
use pyo3::exceptions::{PyOSError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pyo3_stub_gen::{
    define_stub_info_gatherer,
    derive::{gen_stub_pyclass, gen_stub_pyfunction, gen_stub_pymethods},
};

fn py_err(error: Error) -> PyErr {
    match error {
        Error::Io(error) => PyOSError::new_err(error.to_string()),
        error => PyValueError::new_err(error.to_string()),
    }
}

#[gen_stub_pyclass]
#[pyclass(name = "ImageInfo", frozen, skip_from_py_object)]
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
    fn create(path: String, width: u64, height: u64, tile_size: u32) -> PyResult<Self> {
        Ok(Self {
            inner: Some(
                Writer::create_with_tile_size(path, width, height, tile_size).map_err(py_err)?,
            ),
        })
    }
    #[staticmethod]
    #[pyo3(signature = (path, width, height, tile_size, tiles))]
    fn create_from_jpeg_tiles(
        py: Python<'_>,
        path: String,
        width: u64,
        height: u64,
        tile_size: u32,
        tiles: Vec<Vec<Py<PyBytes>>>,
    ) -> PyResult<PyImageInfo> {
        let tiles = tiles
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|tile| tile.bind(py).as_bytes().to_vec())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let info = py.detach(|| {
            Writer::create_from_jpeg_tiles(path, width, height, tile_size, &tiles).map_err(py_err)
        })?;
        Ok(PyImageInfo { inner: info })
    }
    fn write_tile(
        &mut self,
        py: Python<'_>,
        x: u32,
        y: u32,
        data: &Bound<'_, PyBytes>,
    ) -> PyResult<()> {
        let bytes = data.as_bytes().to_vec();
        let mut writer = self
            .inner
            .take()
            .ok_or_else(|| PyValueError::new_err("writer is already finished"))?;
        let (writer, result) = py.detach(|| {
            let result = writer.write_tile(x, y, &bytes);
            (writer, result)
        });
        self.inner = Some(writer);
        result.map_err(py_err)
    }
    fn finish(&mut self, py: Python<'_>) -> PyResult<PyImageInfo> {
        let writer = self
            .inner
            .take()
            .ok_or_else(|| PyValueError::new_err("writer is already finished"))?;
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
    #[pyo3(signature = (x, y, width, height))]
    fn read(&self, py: Python<'_>, x: u64, y: u64, width: u32, height: u32) -> PyResult<PyRegion> {
        let inner = self.inner.clone();
        py.detach(|| {
            inner
                .read(x, y, width, height)
                .map(|region| PyRegion { inner: region })
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
