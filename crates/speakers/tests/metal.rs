#![cfg(target_os = "macos")]
use speakers::metal::{ConvSpec, Convolution, Gpu};
use std::sync::{Arc, Mutex};
#[test]
fn fp16_convolutions_match_captured_ort_layers() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/metal-conv.json")).unwrap();
    let gpu = Arc::new(Mutex::new(Gpu::new().unwrap()));
    for (index, row) in fixture["cases"].as_array().unwrap().iter().enumerate() {
        let vector =
            |name: &str| -> Vec<f32> { serde_json::from_value(row[name].clone()).unwrap() };
        let spec = ConvSpec {
            input: serde_json::from_value(row["shape"].clone()).unwrap(),
            output_channels: row["output_channels"].as_u64().unwrap() as usize,
            kernel: serde_json::from_value(row["kernel"].clone()).unwrap(),
            strides: serde_json::from_value(row["strides"].clone()).unwrap(),
            pads: serde_json::from_value(row["pads"].clone()).unwrap(),
            dilations: serde_json::from_value(row["dilations"].clone()).unwrap(),
            groups: row["groups"].as_u64().unwrap() as usize,
        };
        let convolution =
            Convolution::new(gpu.clone(), &vector("weights"), Some(&vector("bias")), spec).unwrap();
        let result = convolution.run(&vector("input"), spec.input).unwrap();
        let reference = vector("output");
        assert_eq!(result.len(), reference.len());
        let max = result
            .iter()
            .zip(&reference)
            .map(|(a, b)| (a - b).abs())
            .fold(0_f32, f32::max);
        let rms = (result
            .iter()
            .zip(&reference)
            .map(|(a, b)| (a - b).powi(2))
            .sum::<f32>()
            / result.len() as f32)
            .sqrt();
        println!("Conv fixture {index}: max={max} rms={rms}");
        assert!(
            max < 0.001 && rms < 0.0003,
            "case {index}: max={max}, rms={rms}"
        );
    }
    assert_eq!(gpu.lock().unwrap().stats().0, 6);
}
#[test]
fn invalid_geometry_is_rejected_before_dispatch() {
    let spec = ConvSpec {
        input: [1, 3, 2, 2],
        output_channels: 4,
        kernel: [5, 5],
        strides: [1, 1],
        pads: [0, 0],
        dilations: [1, 1],
        groups: 1,
    };
    assert!(spec.output().is_err());
    assert!(ConvSpec { groups: 2, ..spec }.output().is_err());
    assert!(
        ConvSpec {
            strides: [0, 1],
            ..spec
        }
        .output()
        .is_err()
    );
    assert!(
        ConvSpec {
            pads: [usize::MAX, 1],
            ..spec
        }
        .output()
        .is_err()
    );
    assert!(
        ConvSpec {
            dilations: [usize::MAX, 1],
            ..spec
        }
        .output()
        .is_err()
    );
}
