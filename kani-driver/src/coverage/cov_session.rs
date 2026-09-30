// Copyright Kani Contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use std::fs;
use std::fs::File;
use std::io::Write;

use crate::KaniSession;
use crate::harness_runner::HarnessResult;
use crate::project::Project;
use anyhow::{Result, bail};

fn write_coverage_results(outdir: &std::path::Path, results: &[HarnessResult<'_>]) -> Result<()> {
    let mut skipped_harnesses = Vec::new();

    for harness_res in results {
        let Some(cov_results) = harness_res.result.coverage_results.as_ref() else {
            skipped_harnesses.push(harness_res.harness.pretty_name.clone());
            continue;
        };

        let harness_name = harness_res.harness.mangled_name.clone();
        let kaniraw_name = format!("{harness_name}_kaniraw");
        let file_name = outdir.join(kaniraw_name).with_extension("json");
        let mut cov_file = File::create(file_name)?;

        let serialized_data = serde_json::to_string(cov_results)?;
        cov_file.write_all(serialized_data.as_bytes())?;
    }

    if !skipped_harnesses.is_empty() {
        eprintln!(
            "[warn] Skipping coverage output for {} harness(es) with no coverage results: {}",
            skipped_harnesses.len(),
            skipped_harnesses.join(", ")
        );
    }

    Ok(())
}

impl KaniSession {
    /// Saves metadata required for coverage-related features.
    /// At present, this metadata consists of the following:
    ///  - The file names of the project's source code.
    ///
    /// Note: Currently, coverage mappings are not included due to technical
    /// limitations. But this is where we should save them.
    pub fn save_coverage_metadata(&self, project: &Project, stamp: &String) -> Result<()> {
        if project.input.is_none() {
            self.save_coverage_metadata_cargo(project, stamp)
        } else {
            self.save_coverage_metadata_standalone(project, stamp)
        }
    }

    fn save_coverage_metadata_cargo(&self, project: &Project, stamp: &String) -> Result<()> {
        let build_target = env!("TARGET");
        let metadata = self.cargo_metadata(build_target)?;
        let target_dir = self
            .args
            .target_dir
            .as_ref()
            .unwrap_or(&metadata.target_directory.clone().into())
            .clone()
            .join("varies-kani");

        let outdir = target_dir.join(build_target).join(format!("kanicov_{stamp}"));

        // Generally we don't expect this directory to exist, but there's no
        // reason to delete it if it does.
        if !outdir.exists() {
            fs::create_dir(&outdir)?;
        }

        // Collect paths to source files in the project
        let mut source_targets = Vec::new();
        if let Some(metadata) = &project.cargo_metadata {
            for package in &metadata.packages {
                for target in &package.targets {
                    source_targets.push(target.src_path.clone());
                }
            }
        } else {
            bail!("could not find project metadata required for coverage metadata");
        }

        let kanimap_name = format!("kanicov_{stamp}_kanimap");
        let file_name = outdir.join(kanimap_name).with_extension("json");
        let mut kanimap_file = File::create(file_name)?;

        let serialized_data = serde_json::to_string(&source_targets)?;
        kanimap_file.write_all(serialized_data.as_bytes())?;

        Ok(())
    }

    fn save_coverage_metadata_standalone(&self, project: &Project, stamp: &String) -> Result<()> {
        let input = project.input.clone().unwrap().canonicalize().unwrap();
        let input_dir = input.parent().unwrap().to_path_buf();
        let outdir = input_dir.join(format!("kanicov_{stamp}"));

        // Generally we don't expect this directory to exist, but there's no
        // reason to delete it if it does.
        if !outdir.exists() {
            fs::create_dir(&outdir)?;
        }

        // In this case, the source files correspond to the input file
        let source_targets = vec![input];

        let kanimap_name = format!("kanicov_{stamp}_kanimap");
        let file_name = outdir.join(kanimap_name).with_extension("json");
        let mut kanimap_file = File::create(file_name)?;

        let serialized_data = serde_json::to_string(&source_targets)?;
        kanimap_file.write_all(serialized_data.as_bytes())?;

        Ok(())
    }

    /// Saves raw coverage check results required for coverage-related features.
    pub fn save_coverage_results(
        &self,
        project: &Project,
        results: &[HarnessResult<'_>],
        stamp: &String,
    ) -> Result<()> {
        if project.input.is_none() {
            self.save_coverage_results_cargo(results, stamp)
        } else {
            self.save_coverage_results_standalone(project, results, stamp)
        }
    }

    pub fn save_coverage_results_cargo(
        &self,
        results: &[HarnessResult<'_>],
        stamp: &String,
    ) -> Result<()> {
        let build_target = env!("TARGET");
        let metadata = self.cargo_metadata(build_target)?;
        let target_dir = self
            .args
            .target_dir
            .as_ref()
            .unwrap_or(&metadata.target_directory.clone().into())
            .clone()
            .join("varies-kani");

        let outdir = target_dir.join(build_target).join(format!("kanicov_{stamp}"));

        // This directory should have been created by `save_coverage_metadata`,
        // so now we expect it to exist.
        if !outdir.exists() {
            bail!("directory associated to coverage run does not exist")
        }

        write_coverage_results(&outdir, results)?;

        println!("[info] Coverage results saved to {}", &outdir.display());
        Ok(())
    }

    pub fn save_coverage_results_standalone(
        &self,
        project: &Project,
        results: &[HarnessResult<'_>],
        stamp: &String,
    ) -> Result<()> {
        let input = project.input.clone().unwrap().canonicalize().unwrap();
        let input_dir = input.parent().unwrap().to_path_buf();
        let outdir = input_dir.join(format!("kanicov_{stamp}"));

        // This directory should have been created by `save_coverage_metadata`,
        // so now we expect it to exist.
        if !outdir.exists() {
            bail!("directory associated to coverage run does not exist")
        }

        write_coverage_results(&outdir, results)?;

        println!("[info] Coverage results saved to {}", &outdir.display());

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call_cbmc::{FailedProperties, VerificationResult, VerificationStatus};
    use crate::cbmc_output_parser::CheckStatus;
    use crate::coverage::cov_results::{
        CoverageCheck, CoverageRegion, CoverageResults, CoverageTerm,
    };
    use crate::metadata::tests::mock_proof_harness;
    use std::collections::BTreeMap;
    use std::time::Duration;
    use tempfile::tempdir;

    fn sample_coverage_results() -> CoverageResults {
        let check = CoverageCheck::new(
            "harness_fn".to_string(),
            CoverageTerm::Counter(0),
            CoverageRegion { file: "src/lib.rs".to_string(), start: (1, 1), end: (1, 5) },
            CheckStatus::Covered,
        );

        CoverageResults::new(BTreeMap::from([("src/lib.rs".to_string(), vec![check])]))
    }

    fn verification_result_with_coverage(
        coverage_results: Option<CoverageResults>,
    ) -> VerificationResult {
        VerificationResult {
            status: VerificationStatus::Success,
            failed_properties: FailedProperties::None,
            results: Ok(vec![]),
            runtime: Duration::from_secs(0),
            generated_concrete_test: false,
            coverage_results,
        }
    }

    #[test]
    fn skips_harnesses_without_coverage_results() {
        let tempdir = tempdir().unwrap();
        let harness_without_coverage = mock_proof_harness("missing_cov", None, None, None);
        let harness_with_coverage = mock_proof_harness("present_cov", None, None, None);
        let results = vec![
            HarnessResult {
                harness: &harness_without_coverage,
                result: verification_result_with_coverage(None),
            },
            HarnessResult {
                harness: &harness_with_coverage,
                result: verification_result_with_coverage(Some(sample_coverage_results())),
            },
        ];

        write_coverage_results(tempdir.path(), &results).unwrap();

        let written_file = tempdir.path().join("present_cov_kaniraw.json");
        assert!(written_file.exists());
        assert!(!tempdir.path().join("missing_cov_kaniraw.json").exists());

        let saved_results: CoverageResults =
            serde_json::from_reader(File::open(written_file).unwrap()).unwrap();
        assert_eq!(saved_results.data.len(), 1);
    }
}
