#![allow(deprecated)]
//! Narrow owned Core ML layer. Models stay on their creating thread; no Send/Sync claims.
use anyhow::{anyhow, ensure, Result};
use objc2::{
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
    AnyThread,
};
use objc2_core_ml::*;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSURL};
use std::{marker::PhantomData, path::Path, rc::Rc};
pub struct Array {
    pub(crate) inner: Retained<MLMultiArray>,
}
impl Array {
    pub fn zeros(shape: &[usize], dtype: MLMultiArrayDataType) -> Result<Self> {
        ensure!(
            !shape.is_empty() && shape.iter().all(|&d| d > 0),
            "Invalid tensor shape"
        );
        ensure!(
            matches!(
                dtype,
                MLMultiArrayDataType::Float32 | MLMultiArrayDataType::Int32
            ),
            "Unsupported array dtype"
        );
        let dims: Vec<_> = shape.iter().map(|&d| NSNumber::new_usize(d)).collect();
        let a = unsafe {
            MLMultiArray::initWithShape_dataType_error(
                MLMultiArray::alloc(),
                &NSArray::from_retained_slice(&dims),
                dtype,
            )
        }
        .map_err(|e| anyhow!(e.to_string()))?;
        let result = Self { inner: a };
        let shape = result.shape();
        let strides = result.strides();
        let storage = shape
            .iter()
            .zip(strides)
            .map(|(d, s)| (d - 1) * s)
            .sum::<usize>()
            + 1;
        unsafe {
            std::ptr::write_bytes(result.inner.dataPointer().as_ptr(), 0, storage * 4);
        }
        Ok(result)
    }
    pub fn shape(&self) -> Vec<usize> {
        unsafe { self.inner.shape() }
            .iter()
            .map(|v| v.unsignedIntegerValue())
            .collect()
    }
    pub fn strides(&self) -> Vec<usize> {
        unsafe { self.inner.strides() }
            .iter()
            .map(|v| v.unsignedIntegerValue())
            .collect()
    }
    pub fn count(&self) -> usize {
        unsafe { self.inner.count() as usize }
    }
    fn offset(&self, mut index: usize) -> usize {
        let shape = self.shape();
        let strides = self.strides();
        let mut offset = 0;
        for axis in (0..shape.len()).rev() {
            offset += (index % shape[axis]) * strides[axis];
            index /= shape[axis];
        }
        offset
    }
    pub fn get(&self, index: usize) -> f32 {
        assert!(index < self.count());
        let off = self.offset(index);
        unsafe {
            match self.inner.dataType() {
                MLMultiArrayDataType::Float32 => {
                    *self.inner.dataPointer().as_ptr().cast::<f32>().add(off)
                }
                MLMultiArrayDataType::Int32 => {
                    *self.inner.dataPointer().as_ptr().cast::<i32>().add(off) as f32
                }
                _ => panic!("Unsupported Core ML dtype"),
            }
        }
    }
    pub fn set(&self, index: usize, value: f32) {
        assert!(index < self.count());
        let off = self.offset(index);
        unsafe {
            match self.inner.dataType() {
                MLMultiArrayDataType::Float32 => {
                    *self.inner.dataPointer().as_ptr().cast::<f32>().add(off) = value
                }
                MLMultiArrayDataType::Int32 => {
                    *self.inner.dataPointer().as_ptr().cast::<i32>().add(off) = value as i32
                }
                _ => panic!("Unsupported Core ML dtype"),
            }
        }
    }
    pub fn copy_f32(&self, values: &[f32]) -> Result<()> {
        ensure!(
            unsafe { self.inner.dataType() } == MLMultiArrayDataType::Float32
                && self.count() == values.len(),
            "Array copy shape/type mismatch"
        );
        let shape = self.shape();
        let strides = self.strides();
        let mut stride = 1;
        let mut contiguous = true;
        for axis in (0..shape.len()).rev() {
            contiguous &= strides[axis] == stride;
            stride *= shape[axis];
        }
        if contiguous {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    values.as_ptr(),
                    self.inner.dataPointer().as_ptr().cast::<f32>(),
                    values.len(),
                );
            }
        } else {
            for (i, &v) in values.iter().enumerate() {
                self.set(i, v);
            }
        }
        Ok(())
    }
    pub fn values(&self) -> Result<Vec<f32>> {
        ensure!(
            unsafe { self.inner.dataType() } == MLMultiArrayDataType::Float32,
            "Expected Float32 tensor"
        );
        let shape = self.shape();
        let strides = self.strides();
        let ptr = unsafe { self.inner.dataPointer() }.as_ptr().cast::<f32>();
        Ok((0..self.count())
            .map(|mut index| {
                let mut offset = 0;
                for axis in (0..shape.len()).rev() {
                    offset += (index % shape[axis]) * strides[axis];
                    index /= shape[axis];
                }
                unsafe { *ptr.add(offset) }
            })
            .collect())
    }
    pub fn axis_values(&self, axis: usize, fixed: &[usize]) -> Result<Vec<f32>> {
        let shape = self.shape();
        ensure!(
            fixed.len() == shape.len() && axis < shape.len(),
            "Invalid tensor axis"
        );
        ensure!(
            fixed.iter().zip(&shape).all(|(&i, &n)| i < n),
            "Tensor index out of range"
        );
        // The varying axis starts at zero; otherwise the final raw-pointer
        // reads can run past the backing storage even though each fixed index
        // passed the per-axis bounds check above.
        ensure!(fixed[axis] == 0, "Varying tensor axis must start at zero");
        let strides = self.strides();
        let base: usize = fixed.iter().zip(&strides).map(|(a, b)| a * b).sum();
        ensure!(
            unsafe { self.inner.dataType() } == MLMultiArrayDataType::Float32,
            "Expected Float32 tensor"
        );
        Ok((0..shape[axis])
            .map(|i| unsafe {
                *self
                    .inner
                    .dataPointer()
                    .as_ptr()
                    .cast::<f32>()
                    .add(base + i * strides[axis])
            })
            .collect())
    }
}
pub struct Model {
    inner: Retained<MLModel>,
    _thread: PhantomData<Rc<()>>,
}
pub struct Output(Retained<ProtocolObject<dyn MLFeatureProvider>>);
impl Output {
    pub fn array(&self, name: &str) -> Result<Array> {
        let value = unsafe { self.0.featureValueForName(&NSString::from_str(name)) }
            .ok_or_else(|| anyhow!("Missing output {name}"))?;
        let a = unsafe { value.multiArrayValue() }
            .ok_or_else(|| anyhow!("Output {name} is not an array"))?;
        ensure!(
            matches!(
                unsafe { a.dataType() },
                MLMultiArrayDataType::Float32 | MLMultiArrayDataType::Int32
            ),
            "Unsupported output dtype"
        );
        Ok(Array { inner: a })
    }
}
impl Model {
    pub fn load(path: &Path, units: MLComputeUnits) -> Result<Self> {
        let config = unsafe { MLModelConfiguration::new() };
        unsafe {
            config.setComputeUnits(units);
        }
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        let inner = unsafe { MLModel::modelWithContentsOfURL_configuration_error(&url, &config) }
            .map_err(|e| anyhow!("{}: {}", path.display(), e))?;
        Ok(Self {
            inner,
            _thread: PhantomData,
        })
    }
    pub fn predict(&mut self, inputs: &[(&str, &Array)]) -> Result<Output> {
        let keys: Vec<_> = inputs.iter().map(|(k, _)| NSString::from_str(k)).collect();
        let vals: Vec<_> = inputs
            .iter()
            .map(|(_, a)| unsafe { MLFeatureValue::featureValueWithMultiArray(&a.inner) })
            .collect();
        let refs: Vec<Retained<AnyObject>> = vals
            .into_iter()
            .map(|v| v.into_super().into_super())
            .collect();
        let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
        let dict = NSDictionary::from_retained_objects(&key_refs, &refs);
        let input = unsafe {
            MLDictionaryFeatureProvider::initWithDictionary_error(
                MLDictionaryFeatureProvider::alloc(),
                &dict,
            )
        }
        .map_err(|e| anyhow!(e.to_string()))?;
        let result = unsafe {
            self.inner
                .predictionFromFeatures_error(ProtocolObject::from_ref(&*input))
        }
        .map_err(|e| anyhow!(e.to_string()))?;
        Ok(Output(result))
    }
}
#[test]
fn apple_enum_values() {
    assert_eq!(MLComputeUnits::CPUOnly.0, 0);
    assert_eq!(MLComputeUnits::CPUAndNeuralEngine.0, 3);
}

#[test]
fn axis_reads_reject_nonzero_varying_origin() {
    let a = Array::zeros(&[2, 3], MLMultiArrayDataType::Float32).unwrap();
    a.copy_f32(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0]).unwrap();
    assert_eq!(a.axis_values(1, &[1, 0]).unwrap(), [3.0, 4.0, 5.0]);
    assert!(a.axis_values(1, &[1, 2]).is_err());
    assert!(a.axis_values(1, &[2, 0]).is_err());
}
