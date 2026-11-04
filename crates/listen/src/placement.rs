//! Core ML's compute-plan assignment, not a claim of measured hardware utilization.
use anyhow::{anyhow, Result};
use objc2::runtime::AnyObject;
use objc2_core_ml::*;
use objc2_foundation::{NSString, NSURL};
use std::{collections::BTreeMap, path::Path, time::Duration};
fn walk(
    plan: &MLComputePlan,
    block: &MLModelStructureProgramBlock,
    counts: &mut BTreeMap<String, usize>,
) {
    for op in unsafe { block.operations() } {
        if let Some(usage) = unsafe { plan.computeDeviceUsageForMLProgramOperation(&op) } {
            let device = unsafe { usage.preferredComputeDevice() };
            let object: &AnyObject = device.as_ref();
            *counts
                .entry(object.class().name().to_string_lossy().into_owned())
                .or_default() += 1;
        }
        for nested in unsafe { op.blocks() } {
            walk(plan, &nested, counts);
        }
    }
}
pub fn compute_plan(path: &Path, units: MLComputeUnits) -> Result<BTreeMap<String, usize>> {
    let config = unsafe { MLModelConfiguration::new() };
    unsafe {
        config.setComputeUnits(units);
    }
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    let (tx, rx) = std::sync::mpsc::channel();
    let handler = block2::RcBlock::new(
        move |plan: *mut MLComputePlan, error: *mut objc2_foundation::NSError| {
            let result = if let Some(plan) = unsafe { plan.as_ref() } {
                let mut counts = BTreeMap::new();
                if let Some(program) = unsafe { plan.modelStructure().program() } {
                    for function in unsafe { program.functions().allValues() } {
                        let block = unsafe { function.block() };
                        walk(plan, &block, &mut counts);
                    }
                }
                Ok(counts)
            } else {
                Err(anyhow!(
                    "Compute plan: {}",
                    unsafe { error.as_ref() }
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "No result".into())
                ))
            };
            let _ = tx.send(result);
        },
    );
    unsafe {
        MLComputePlan::loadContentsOfURL_configuration_completionHandler(&url, &config, &handler);
    }
    rx.recv_timeout(Duration::from_secs(60))
        .map_err(|e| anyhow!(e))?
}
