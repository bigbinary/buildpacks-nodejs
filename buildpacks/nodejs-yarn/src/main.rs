use crate::yarn::Yarn;
use heroku_nodejs_utils::inv::Inventory;
use heroku_nodejs_utils::package_json::{PackageJson, PackageJsonError};
use heroku_nodejs_utils::vrs::{Requirement, VersionError};
use libcnb::build::{BuildContext, BuildResult, BuildResultBuilder};
use libcnb::data::build_plan::BuildPlanBuilder;
use libcnb::data::launch::{LaunchBuilder, ProcessBuilder};
use libcnb::data::process_type;
use libcnb::detect::{DetectContext, DetectResult, DetectResultBuilder};
use libcnb::generic::GenericMetadata;
use libcnb::generic::GenericPlatform;
use libcnb::layer_env::Scope;
use libcnb::{buildpack_main, Buildpack, Env};
use libherokubuildpack::log::{log_error, log_header, log_info};
use thiserror::Error;

use crate::configure_yarn_cache::{configure_yarn_cache, DepsLayerError};
use crate::install_yarn::{install_yarn, CliLayerError};
use heroku_nodejs_utils::buildplan::{
    read_node_build_scripts_metadata, NodeBuildScriptsMetadataError,
    NODE_BUILD_SCRIPTS_BUILD_PLAN_NAME,
};
#[cfg(test)]
use indoc as _;
#[cfg(test)]
use libcnb_test as _;
#[cfg(test)]
use test_support as _;
#[cfg(test)]
use ureq as _;

mod cfg;
mod cmd;
mod configure_yarn_cache;
mod install_yarn;
mod yarn;

const INVENTORY: &str = include_str!("../inventory.toml");
const DEFAULT_YARN_REQUIREMENT: &str = "1.22.x";

struct YarnBuildpack;

impl Buildpack for YarnBuildpack {
    type Platform = GenericPlatform;
    type Metadata = GenericMetadata;
    type Error = YarnBuildpackError;

    fn detect(&self, context: DetectContext<Self>) -> libcnb::Result<DetectResult, Self::Error> {
        context
            .app_dir
            .join("yarn.lock")
            .exists()
            .then(|| {
                DetectResultBuilder::pass()
                    .build_plan(
                        BuildPlanBuilder::new()
                            .provides("yarn")
                            .provides("node_modules")
                            .provides(NODE_BUILD_SCRIPTS_BUILD_PLAN_NAME)
                            .requires("node")
                            .requires("yarn")
                            .requires("node_modules")
                            .requires(NODE_BUILD_SCRIPTS_BUILD_PLAN_NAME)
                            .build(),
                    )
                    .build()
            })
            .unwrap_or_else(|| DetectResultBuilder::fail().build())
    }

    fn build(&self, context: BuildContext<Self>) -> libcnb::Result<BuildResult, Self::Error> {
        let mut env = Env::from_current();
        let pkg_json = PackageJson::read(context.app_dir.join("package.json"))
            .map_err(YarnBuildpackError::PackageJson)?;
        let node_build_scripts_metadata = read_node_build_scripts_metadata(&context.buildpack_plan)
            .map_err(YarnBuildpackError::NodeBuildScriptsMetadata)?;

        let yarn_version = match cmd::yarn_version(&env) {
            // Install yarn if it's not present.
            Err(cmd::Error::Spawn(_)) => {
                log_header("Detecting yarn CLI version to install");

                let inventory: Inventory =
                    toml::from_str(INVENTORY).map_err(YarnBuildpackError::InventoryParse)?;

                let requested_yarn_cli_range = match cfg::requested_yarn_range(&pkg_json) {
                    None => {
                        log_info("No yarn engine range detected in package.json, using default ({DEFAULT_YARN_REQUIREMENT})");
                        Requirement::parse(DEFAULT_YARN_REQUIREMENT)
                            .map_err(YarnBuildpackError::YarnDefaultParse)?
                    }
                    Some(requirement) => {
                        log_info(format!(
                            "Detected yarn engine version range {requirement} in package.json"
                        ));
                        requirement
                    }
                };

                let yarn_cli_release = inventory.resolve(&requested_yarn_cli_range).ok_or(
                    YarnBuildpackError::YarnVersionResolve(requested_yarn_cli_range),
                )?;

                log_info(format!(
                    "Resolved yarn CLI version: {}",
                    yarn_cli_release.version
                ));

                log_header("Installing yarn CLI");
                let yarn_env = install_yarn(&context, yarn_cli_release)?;
                env = yarn_env.apply(Scope::Build, &env);

                cmd::yarn_version(&env).map_err(YarnBuildpackError::YarnVersionDetect)?
            }
            // Use the existing yarn installation if it is present.
            Ok(version) => version,
            err => err.map_err(YarnBuildpackError::YarnVersionDetect)?,
        };

        let yarn = Yarn::from_major(yarn_version.major())
            .ok_or_else(|| YarnBuildpackError::YarnVersionUnsupported(yarn_version.major()))?;

        log_info(format!("Yarn CLI operating in yarn {yarn_version} mode."));

        log_header("Setting up yarn dependency cache");
        cmd::yarn_disable_global_cache(&yarn, &env)
            .map_err(YarnBuildpackError::YarnDisableGlobalCache)?;
        let zero_install = cfg::cache_populated(
            &cmd::yarn_get_cache(&yarn, &env).map_err(YarnBuildpackError::YarnCacheGet)?,
        );
        if zero_install {
            log_info("Yarn zero-install detected. Skipping dependency cache.");
        } else {
            configure_yarn_cache(&context, &yarn, &env)?;
        }

        log_header("Installing dependencies");
        cmd::yarn_install(&yarn, zero_install, &env).map_err(YarnBuildpackError::YarnInstall)?;

        log_header("Running scripts");
        let scripts = pkg_json.build_scripts();
        if scripts.is_empty() {
            log_info("No build scripts found");
        } else {
            for script in scripts {
                if let Some(false) = node_build_scripts_metadata.enabled {
                    log_info(format!(
                        "! Not running `{script}` as it was disabled by a participating buildpack",
                    ));
                } else {
                    log_info(format!("Running `{script}` script"));
                    cmd::yarn_run(&env, &script).map_err(YarnBuildpackError::BuildScript)?;
                }
            }
        }

        if context.app_dir.join("Procfile").exists() {
            log_info("Skipping default web process (Procfile detected)");
            BuildResultBuilder::new().build()
        } else if pkg_json.has_start_script() {
            BuildResultBuilder::new()
                .launch(
                    LaunchBuilder::new()
                        .process(
                            ProcessBuilder::new(process_type!("web"), ["yarn", "start"])
                                .default(true)
                                .build(),
                        )
                        .build(),
                )
                .build()
        } else {
            BuildResultBuilder::new().build()
        }
    }

    fn on_error(&self, error: libcnb::Error<Self::Error>) {
        match error {
            libcnb::Error::BuildpackError(bp_err) => {
                let err_string = bp_err.to_string();
                match bp_err {
                    YarnBuildpackError::BuildScript(_) => {
                        log_error("Yarn build script error", err_string);
                    }
                    YarnBuildpackError::CliLayer(_) => {
                        log_error("Yarn distribution layer error", err_string);
                    }
                    YarnBuildpackError::DepsLayer(_) => {
                        log_error("Yarn dependency layer error", err_string);
                    }
                    YarnBuildpackError::InventoryParse(_) => {
                        log_error("Yarn inventory parse error", err_string);
                    }
                    YarnBuildpackError::PackageJson(_) => {
                        log_error("Yarn package.json error", err_string);
                    }
                    YarnBuildpackError::YarnCacheGet(_)
                    | YarnBuildpackError::YarnDisableGlobalCache(_) => {
                        log_error("Yarn cache error", err_string);
                    }
                    YarnBuildpackError::YarnInstall(_) => {
                        log_error("Yarn install error", err_string);
                    }
                    YarnBuildpackError::YarnVersionDetect(_)
                    | YarnBuildpackError::YarnVersionResolve(_)
                    | YarnBuildpackError::YarnVersionUnsupported(_)
                    | YarnBuildpackError::YarnDefaultParse(_) => {
                        log_error("Yarn version error", err_string);
                    }
                    YarnBuildpackError::NodeBuildScriptsMetadata(_) => {
                        log_error("Yarn buildplan error", err_string);
                    }
                }
            }
            err => {
                log_error("Yarn internal buildpack error", err.to_string());
            }
        }
    }
}

#[derive(Error, Debug)]
enum YarnBuildpackError {
    #[error("Couldn't run build script: {0}")]
    BuildScript(cmd::Error),
    #[error("{0}")]
    CliLayer(#[from] CliLayerError),
    #[error("{0}")]
    DepsLayer(#[from] DepsLayerError),
    #[error("Couldn't parse yarn inventory: {0}")]
    InventoryParse(toml::de::Error),
    #[error("Couldn't parse package.json: {0}")]
    PackageJson(PackageJsonError),
    #[error("Couldn't read yarn cache folder: {0}")]
    YarnCacheGet(cmd::Error),
    #[error("Couldn't disable yarn global cache: {0}")]
    YarnDisableGlobalCache(cmd::Error),
    #[error("Yarn install error: {0}")]
    YarnInstall(cmd::Error),
    #[error("Couldn't determine yarn version: {0}")]
    YarnVersionDetect(cmd::Error),
    #[error("Unsupported yarn version: {0}")]
    YarnVersionUnsupported(u64),
    #[error("Couldn't resolve yarn version requirement ({0}) to a known yarn version")]
    YarnVersionResolve(Requirement),
    #[error("Couldn't parse yarn default version range: {0}")]
    YarnDefaultParse(VersionError),
    #[error("Couldn't parse metadata for the buildplan named {NODE_BUILD_SCRIPTS_BUILD_PLAN_NAME}: {0:?}")]
    NodeBuildScriptsMetadata(NodeBuildScriptsMetadataError),
}

impl From<YarnBuildpackError> for libcnb::Error<YarnBuildpackError> {
    fn from(e: YarnBuildpackError) -> Self {
        libcnb::Error::BuildpackError(e)
    }
}

buildpack_main!(YarnBuildpack);
