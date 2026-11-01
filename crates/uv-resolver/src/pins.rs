use std::collections::hash_map::Entry;

use rustc_hash::FxHashMap;

use uv_distribution_filename::WheelFilename;
use uv_distribution_types::{Dist, DistributionId, Identifier, ResolvedDist};
use uv_normalize::PackageName;
use uv_pep440::Version;

use crate::ResolveError;
use crate::candidate_selector::Candidate;
use crate::prioritized_distribution::CompatibleDist;
use crate::resolver::{RegisteredMetadata, RegisteredVariantsJson};

#[derive(Clone, Debug)]
enum FilePin<'index> {
    Registry {
        /// The concrete distribution chosen for installation and locking.
        dist: ResolvedDist,
        /// The concrete distribution whose metadata is used during resolution.
        metadata: PinMetadata<'index>,
        /// The index-level variant metadata for the selected package version.
        variants_json: Option<RegisteredVariantsJson<'index>>,
    },
    Url(RegisteredMetadata<'index>),
}

#[derive(Clone, Debug)]
enum PinMetadata<'index> {
    /// Proxy selection and direct-only resolution do not require a metadata request.
    Unrequested(DistributionId),
    Registered(RegisteredMetadata<'index>),
}

/// The artifacts and metadata selected for package versions within a fork.
#[derive(Clone, Debug, Default)]
pub(crate) struct FilePins<'index>(FxHashMap<(PackageName, Version), FilePin<'index>>);

impl<'index> FilePins<'index> {
    /// Pin a registry candidate, registering its metadata at most once in this fork.
    ///
    /// A proxy may pin a provisional artifact. The first metadata request fixes the artifact
    /// for that package version within the fork.
    pub(crate) fn insert(
        &mut self,
        candidate: &Candidate,
        dist: &CompatibleDist,
        variants_json: Option<RegisteredVariantsJson<'index>>,
        request: Option<impl FnOnce() -> Result<RegisteredMetadata<'index>, ResolveError>>,
    ) -> Result<(), ResolveError> {
        match self
            .0
            .entry((candidate.name().clone(), candidate.version().clone()))
        {
            Entry::Occupied(mut entry) => {
                if let Some(request) = request
                    && let FilePin::Registry {
                        metadata: PinMetadata::Unrequested(_),
                        ..
                    } = entry.get()
                {
                    // A proxy can choose a different wheel before the runtime version is known.
                    // Pin the artifact and its sidecar together with the first metadata request.
                    entry.insert(FilePin::Registry {
                        dist: dist.for_installation().to_owned(),
                        metadata: PinMetadata::Registered(request()?),
                        variants_json,
                    });
                }
            }
            Entry::Vacant(entry) => {
                let metadata = if let Some(request) = request {
                    PinMetadata::Registered(request()?)
                } else {
                    PinMetadata::Unrequested(dist.for_resolution().distribution_id())
                };
                entry.insert(FilePin::Registry {
                    dist: dist.for_installation().to_owned(),
                    metadata,
                    variants_json,
                });
            }
        }
        Ok(())
    }

    /// Retain the metadata used to select a URL package's version.
    pub(crate) fn insert_url(
        &mut self,
        name: &PackageName,
        version: &Version,
        metadata: RegisteredMetadata<'index>,
    ) {
        self.0
            .entry((name.clone(), version.clone()))
            .or_insert(FilePin::Url(metadata));
    }

    /// Selected ABI variant wheels whose dependencies may appear in a resolution error.
    pub(crate) fn abi_variant_wheels(&self) -> impl Iterator<Item = &WheelFilename> {
        self.0.values().filter_map(|pin| {
            let FilePin::Registry {
                dist: ResolvedDist::Installable { dist, .. },
                variants_json: Some(variants_json),
                ..
            } = pin
            else {
                return None;
            };
            let Dist::Built(wheel) = dist.as_ref() else {
                return None;
            };
            let filename = wheel.wheel_filename();
            let label = filename.variant()?;
            let response = variants_json.wait();
            let metadata = response.as_ref().as_ref().ok()?;
            metadata.has_abi_dependency(label).then_some(filename)
        })
    }

    /// Return the pinned registry artifact, if one exists.
    pub(crate) fn get(&self, name: &PackageName, version: &Version) -> Option<&ResolvedDist> {
        match self.0.get(&(name.clone(), version.clone()))? {
            FilePin::Registry { dist, .. } => Some(dist),
            FilePin::Url(_) => None,
        }
    }

    /// Return the metadata registered when selecting this package version.
    pub(crate) fn metadata(
        &self,
        name: &PackageName,
        version: &Version,
    ) -> Option<&RegisteredMetadata<'index>> {
        match self.0.get(&(name.clone(), version.clone()))? {
            FilePin::Registry {
                metadata: PinMetadata::Registered(metadata),
                ..
            }
            | FilePin::Url(metadata) => Some(metadata),
            FilePin::Registry {
                metadata: PinMetadata::Unrequested(_),
                ..
            } => None,
        }
    }

    /// Return the variant metadata registered when selecting this package version.
    pub(crate) fn variants_json(
        &self,
        name: &PackageName,
        version: &Version,
    ) -> Option<&RegisteredVariantsJson<'index>> {
        match self.0.get(&(name.clone(), version.clone()))? {
            FilePin::Registry { variants_json, .. } => variants_json.as_ref(),
            FilePin::Url(_) => None,
        }
    }

    /// Return the pinned registry artifact and its metadata identity in a single lookup.
    pub(crate) fn dist_and_id(
        &self,
        name: &PackageName,
        version: &Version,
    ) -> Option<(&ResolvedDist, &DistributionId)> {
        match self.0.get(&(name.clone(), version.clone()))? {
            FilePin::Registry { dist, metadata, .. } => Some((
                dist,
                match metadata {
                    PinMetadata::Unrequested(id) => id,
                    PinMetadata::Registered(metadata) => metadata.id(),
                },
            )),
            FilePin::Url(_) => None,
        }
    }
}
