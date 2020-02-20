#![deny(rust_2018_idioms)]
#![warn(clippy::pedantic)]

#[path = "../error.rs"]
mod error;

#[macro_use]
extern crate log;

use crate::error::Result;
use chrono::{DateTime, Utc};
use data_store_version::Version as DataVersion;
use semver::Version as SemVer;
use simplelog::{Config as LogConfig, LevelFilter, TermLogger, TerminalMode};
use snafu::{ErrorCompat, OptionExt, ResultExt};
use std::fs;
use std::path::PathBuf;
use structopt::StructOpt;
use update_metadata::{Images, Manifest, Release, Update};

#[derive(Debug, StructOpt)]
struct GeneralArgs {
    // metadata file to create/modify
    file: PathBuf,
}

#[derive(Debug, StructOpt)]
struct AddUpdateArgs {
    // metadata file to create/modify
    file: PathBuf,

    // image 'variant', eg. 'aws-k8s'
    #[structopt(short = "f", long = "variant")]
    variant: String,

    // image version
    #[structopt(short = "v", long = "version")]
    image_version: SemVer,

    // architecture image is built for
    #[structopt(short = "a", long = "arch")]
    arch: String,

    // corresponding datastore version for this image
    #[structopt(short = "d", long = "data-version")]
    datastore_version: DataVersion,

    // maximum valid version
    #[structopt(short = "m", long = "max-version")]
    max_version: Option<SemVer>,

    // root image target name
    #[structopt(short = "r", long = "root")]
    root: String,

    // boot image target name
    #[structopt(short = "b", long = "boot")]
    boot: String,

    // verity "hash" image target name
    #[structopt(short = "h", long = "hash")]
    hash: String,
}

impl AddUpdateArgs {
    fn run(self) -> Result<()> {
        let mut manifest: Manifest = match update_metadata::load_file(&self.file) {
            Ok(m) => m,
            _ => Manifest::default(), // TODO only if EEXIST
        };

        manifest.add_update(
            self.image_version,
            self.max_version,
            self.datastore_version,
            self.arch,
            self.variant,
            Images {
                root: self.root,
                boot: self.boot,
                hash: self.hash,
            },
        )?;
        update_metadata::write_file(&self.file, &manifest)?;
        Ok(())
    }
}

#[derive(Debug, StructOpt)]
struct RemoveUpdateArgs {
    // metadata file to create/modify
    file: PathBuf,

    // image 'variant', eg. 'aws-k8s'
    #[structopt(short = "l", long = "variant")]
    variant: String,

    // image version
    #[structopt(short = "v", long = "version")]
    image_version: SemVer,

    // architecture image is built for
    #[structopt(short = "a", long = "arch")]
    arch: String,

    // Whether to clean up datastore mappings that no longer reference an
    // existing update. Migration paths for such datastore versions are
    // preserved.
    // This should _only_ be used if there are no existing users of the
    // specified Bottlerocket image version.
    #[structopt(short, long)]
    cleanup: bool,
}

impl RemoveUpdateArgs {
    fn run(&self) -> Result<()> {
        let mut manifest: Manifest = update_metadata::load_file(&self.file)?;
        // Remove any update that exactly matches the specified update
        manifest.updates.retain(|update| {
            update.arch != self.arch
                || update.variant != self.variant
                || update.version != self.image_version
        });
        if self.cleanup {
            let remaining: Vec<&Update> = manifest
                .updates
                .iter()
                .filter(|update| update.version == self.image_version)
                .collect();
            if remaining.is_empty() {
                manifest.datastore_versions.remove(&self.image_version);
            } else {
                info!(
                    "Cleanup skipped; {} {} updates remain",
                    remaining.len(),
                    self.image_version
                );
            }
        }
        // Note: We don't revert the maximum version on removal
        update_metadata::write_file(&self.file, &manifest)?;
        if let Some(current) = manifest.updates.first() {
            info!(
                "Update {}-{}-{} removed. Current maximum version: {}",
                self.arch, self.variant, self.image_version, current.version
            );
        } else {
            info!(
                "Update {}-{}-{} removed. No remaining updates",
                self.arch, self.variant, self.image_version
            );
        }
        Ok(())
    }
}

#[derive(Debug, StructOpt)]
struct WaveArgs {
    // metadata file to create/modify
    file: PathBuf,

    // image 'variant', eg. 'aws-k8s'
    #[structopt(short = "l", long = "variant")]
    variant: String,

    // image version
    #[structopt(short = "v", long = "version")]
    image_version: SemVer,

    // architecture image is built for
    #[structopt(short = "a", long = "arch")]
    arch: String,

    // start bound id for this wave (0 <= x < 2048)
    #[structopt(short = "b", long = "bound-id")]
    bound: u32,

    // start time for this wave
    #[structopt(short = "s", long = "start-time")]
    start: Option<DateTime<Utc>>,
}

impl WaveArgs {
    fn add(self) -> Result<()> {
        let mut manifest: Manifest = update_metadata::load_file(&self.file)?;
        let start = self.start.context(error::WaveStartArg)?;
        let num_matching = manifest.add_wave(
            self.variant,
            self.arch,
            self.image_version,
            self.bound,
            start,
        )?;
        if num_matching > 1 {
            warn!("Multiple matching updates for wave - this is weird but not a disaster");
        }
        update_metadata::write_file(&self.file, &manifest)?;
        Ok(())
    }

    fn remove(self) -> Result<()> {
        let mut manifest: Manifest = update_metadata::load_file(&self.file)?;
        manifest.remove_wave(self.variant, self.arch, self.image_version, self.bound)?;
        update_metadata::write_file(&self.file, &manifest)?;
        Ok(())
    }
}

#[derive(Debug, StructOpt)]
struct MigrationArgs {
    // file to get migrations from (probably Release.toml)
    #[structopt(short = "f", long = "from")]
    from: PathBuf,

    // file to write migrations to (probably manifest.json)
    #[structopt(short = "t", long = "to")]
    to: PathBuf,
}

impl MigrationArgs {
    fn set(self) -> Result<()> {
        // Load the file we will be writing to
        let mut manifest: Manifest = update_metadata::load_file(&self.to)?;

        // Load the file we will be reading from
        let release_data =
            fs::read_to_string(&self.from).context(error::ConfigRead { path: &self.from })?;
        let release: Release =
            toml::from_str(&release_data).context(error::ReleaseParse { path: &self.from })?;

        // Replace the manifest 'migrations' section with the new data
        manifest.migrations = release.migrations;

        update_metadata::write_file(&self.to, &manifest)?;
        Ok(())
    }
}

#[derive(Debug, StructOpt)]
struct MaxVersionArgs {
    // metadata file to create/modify
    file: PathBuf,

    // maximum valid version
    #[structopt(short, long)]
    max_version: SemVer,
}

impl MaxVersionArgs {
    fn run(self) -> Result<()> {
        let mut manifest: Manifest = update_metadata::load_file(&self.file)?;
        manifest.update_max_version(&self.max_version, None, None);
        update_metadata::write_file(&self.file, &manifest)?;
        Ok(())
    }
}

#[derive(Debug, StructOpt)]
struct MappingArgs {
    // metadata file to create/modify
    file: PathBuf,

    #[structopt(short, long)]
    image_version: SemVer,

    #[structopt(short, long)]
    data_version: DataVersion,
}

impl MappingArgs {
    fn run(self) -> Result<()> {
        let mut manifest: Manifest = update_metadata::load_file(&self.file)?;
        let version = self.image_version.clone();
        let old = manifest
            .datastore_versions
            .insert(self.image_version, self.data_version);
        if let Some(old) = old {
            warn!(
                "Warning: New mapping ({},{}) replaced old mapping ({},{})",
                version, self.data_version, version, old
            );
        }
        update_metadata::write_file(&self.file, &manifest)?;
        Ok(())
    }
}

#[derive(Debug, StructOpt)]
#[structopt(rename_all = "kebab-case")]
enum Command {
    /// Create an empty manifest
    Init(GeneralArgs),
    /// Add a new update to the manifest, not including wave information
    AddUpdate(AddUpdateArgs),
    /// Add a (bound_id, time) wave to an existing update
    AddWave(WaveArgs),
    /// Add a image_version:data_store_version mapping to the manifest
    AddVersionMapping(MappingArgs),
    /// Set the global maximum image version
    SetMaxVersion(MaxVersionArgs),
    /// Remove an update from the manifest, including wave information
    RemoveUpdate(RemoveUpdateArgs),
    /// Remove a (bound_id, time) wave from an update
    RemoveWave(WaveArgs),
    /// Copy the migrations from an input file to an output file
    SetMigrations(MigrationArgs),
    /// Validate a manifest file, but make no changes
    Validate(GeneralArgs),
}

fn main_inner() -> Result<()> {
    // TerminalMode::Mixed will send errors to stderr and anything less to stdout.
    TermLogger::init(LevelFilter::Info, LogConfig::default(), TerminalMode::Mixed)
        .context(error::Logger)?;

    match Command::from_args() {
        Command::Init(args) => {
            match update_metadata::write_file(&args.file, &Manifest::default()) {
                Ok(_) => Ok(()),
                Err(e) => Err(error::Error::UpdateMetadata { source: e }),
            }
        }
        Command::AddUpdate(args) => args.run(),
        Command::AddWave(args) => args.add(),
        Command::AddVersionMapping(args) => args.run(),
        Command::SetMaxVersion(args) => args.run(),
        Command::RemoveUpdate(args) => args.run(),
        Command::RemoveWave(args) => args.remove(),
        Command::SetMigrations(args) => args.set(),
        Command::Validate(args) => match update_metadata::load_file(&args.file) {
            Ok(_) => Ok(()),
            Err(e) => Err(error::Error::UpdateMetadata { source: e }),
        },
    }
}

fn main() -> ! {
    std::process::exit(match main_inner() {
        Ok(()) => 0,
        Err(err) => {
            error!("{}", err);
            if let Some(var) = std::env::var_os("RUST_BACKTRACE") {
                if var != "0" {
                    if let Some(backtrace) = err.backtrace() {
                        error!("\n{:?}", backtrace);
                    }
                }
            }
            1
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use std::path::Path;
    use std::str::FromStr;
    use tempfile::NamedTempFile;

    #[test]
    // Ensure that we can update a blank manifest
    fn test_migration_copy() -> Result<()> {
        let release_path = "tests/data/release.toml";
        let temp_manifest = NamedTempFile::new().context(error::TmpFileCreate)?;

        // Create a new blank manifest
        update_metadata::write_file(&temp_manifest.path(), &Manifest::default()).unwrap();

        // Copy the migration data to the new manifest
        MigrationArgs {
            from: PathBuf::from(&release_path),
            to: PathBuf::from(temp_manifest.path()),
        }
        .set()
        .unwrap();

        // Make sure the manifest has the correct releases
        let manifest: Manifest = update_metadata::load_file(&temp_manifest.path()).unwrap();
        let release_data = fs::read_to_string(&release_path).unwrap();
        let release: Release = toml::from_str(&release_data).unwrap();
        assert_eq!(manifest.migrations, release.migrations);
        Ok(())
    }

    #[test]
    // Ensure that we can update an existing manifest
    fn test_migration_update() -> Result<()> {
        let release_path = "tests/data/release.toml";
        let example_manifest = "tests/data/example.json";

        // Write example data to temp manifest so we dont' overwrite the file
        // when we call MigrationsArgs.set() below
        let temp_manifest = NamedTempFile::new().context(error::TmpFileCreate)?;
        let example_data = fs::read_to_string(&example_manifest).unwrap();
        fs::write(&temp_manifest, &example_data).unwrap();

        // Copy the migration data to the existing manifest
        MigrationArgs {
            from: PathBuf::from(&release_path),
            to: PathBuf::from(&temp_manifest.path()),
        }
        .set()
        .unwrap();

        // Make sure the manifest has the correct releases
        let manifest: Manifest =
            update_metadata::load_file(Path::new(&temp_manifest.path())).unwrap();
        let release_data = fs::read_to_string(&release_path).unwrap();
        let release: Release = toml::from_str(&release_data).unwrap();
        assert_eq!(manifest.migrations, release.migrations);
        Ok(())
    }

    #[test]
    fn max_versions() -> Result<()> {
        let tmpfd = NamedTempFile::new().context(error::TmpFileCreate)?;
        AddUpdateArgs {
            file: PathBuf::from(tmpfd.path()),
            variant: String::from("yum"),
            arch: String::from("x86_64"),
            image_version: SemVer::parse("1.2.3").unwrap(),
            max_version: Some(SemVer::parse("1.2.3").unwrap()),
            datastore_version: DataVersion::from_str("1.0").unwrap(),
            boot: String::from("boot"),
            root: String::from("root"),
            hash: String::from("hash"),
        }
        .run()
        .unwrap();
        AddUpdateArgs {
            file: PathBuf::from(tmpfd.path()),
            variant: String::from("yum"),
            arch: String::from("x86_64"),
            image_version: SemVer::parse("1.2.5").unwrap(),
            max_version: Some(SemVer::parse("1.2.3").unwrap()),
            datastore_version: DataVersion::from_str("1.0").unwrap(),
            boot: String::from("boot"),
            root: String::from("root"),
            hash: String::from("hash"),
        }
        .run()
        .unwrap();
        AddUpdateArgs {
            file: PathBuf::from(tmpfd.path()),
            variant: String::from("yum"),
            arch: String::from("x86_64"),
            image_version: SemVer::parse("1.2.4").unwrap(),
            max_version: Some(SemVer::parse("1.2.4").unwrap()),
            datastore_version: DataVersion::from_str("1.0").unwrap(),
            boot: String::from("boot"),
            root: String::from("root"),
            hash: String::from("hash"),
        }
        .run()
        .unwrap();

        let m: Manifest = update_metadata::load_file(tmpfd.path())?;
        for u in m.updates {
            assert!(u.max_version == SemVer::parse("1.2.4").unwrap());
        }
        Ok(())
    }

    #[test]
    fn datastore_mapping() -> Result<()> {
        let tmpfd = NamedTempFile::new().context(error::TmpFileCreate)?;
        AddUpdateArgs {
            file: PathBuf::from(tmpfd.path()),
            variant: String::from("yum"),
            arch: String::from("x86_64"),
            image_version: SemVer::parse("1.2.3").unwrap(),
            max_version: Some(SemVer::parse("1.2.3").unwrap()),
            datastore_version: DataVersion::from_str("1.0").unwrap(),
            boot: String::from("boot"),
            root: String::from("root"),
            hash: String::from("hash"),
        }
        .run()
        .unwrap();
        AddUpdateArgs {
            file: PathBuf::from(tmpfd.path()),
            variant: String::from("yum"),
            arch: String::from("x86_64"),
            image_version: SemVer::parse("1.2.5").unwrap(),
            max_version: Some(SemVer::parse("1.2.3").unwrap()),
            datastore_version: DataVersion::from_str("1.1").unwrap(),
            boot: String::from("boot"),
            root: String::from("root"),
            hash: String::from("hash"),
        }
        .run()
        .unwrap();
        AddUpdateArgs {
            file: PathBuf::from(tmpfd.path()),
            variant: String::from("yum"),
            arch: String::from("x86_64"),
            image_version: SemVer::parse("1.2.4").unwrap(),
            max_version: Some(SemVer::parse("1.2.4").unwrap()),
            datastore_version: DataVersion::from_str("1.0").unwrap(),
            boot: String::from("boot"),
            root: String::from("root"),
            hash: String::from("hash"),
        }
        .run()
        .unwrap();

        // TODO this needs to test against arch and variant not being considered
        RemoveUpdateArgs {
            file: PathBuf::from(tmpfd.path()),
            variant: String::from("yum"),
            arch: String::from("x86_64"),
            image_version: SemVer::parse("1.2.4").unwrap(),
            cleanup: true,
        }
        .run()
        .unwrap();

        let m: Manifest = update_metadata::load_file(tmpfd.path())?;
        assert!(m
            .datastore_versions
            .contains_key(&SemVer::parse("1.2.3").unwrap()));
        Ok(())
    }

    #[test]
    fn ordered_waves() -> Result<()> {
        let tmpfd = NamedTempFile::new().context(error::TmpFileCreate)?;
        AddUpdateArgs {
            file: PathBuf::from(tmpfd.path()),
            variant: String::from("yum"),
            arch: String::from("x86_64"),
            image_version: SemVer::parse("1.2.3").unwrap(),
            max_version: Some(SemVer::parse("1.2.3").unwrap()),
            datastore_version: DataVersion::from_str("1.0").unwrap(),
            boot: String::from("boot"),
            root: String::from("root"),
            hash: String::from("hash"),
        }
        .run()
        .unwrap();

        WaveArgs {
            file: PathBuf::from(tmpfd.path()),
            variant: String::from("yum"),
            arch: String::from("x86_64"),
            image_version: SemVer::parse("1.2.3").unwrap(),
            bound: 1024,
            start: Some(Utc::now()),
        }
        .add()
        .unwrap();

        assert!(WaveArgs {
            file: PathBuf::from(tmpfd.path()),
            variant: String::from("yum"),
            arch: String::from("x86_64"),
            image_version: SemVer::parse("1.2.3").unwrap(),
            bound: 1536,
            start: Some(Utc::now() - Duration::hours(1)),
        }
        .add()
        .is_err());

        Ok(())
    }
}
