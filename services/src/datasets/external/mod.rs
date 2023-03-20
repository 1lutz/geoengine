#[cfg(feature = "nfdi")]
pub mod aruna;
#[cfg(feature = "nfdi")]
pub mod gbif;
#[cfg(feature = "nfdi")]
pub mod gfbio_abcd; // TODO: rename to "gfbio_abcd"?
#[cfg(feature = "nfdi")]
pub mod gfbio_collections;
pub mod mock;
#[cfg(feature = "nature40")]
pub mod nature40;
#[cfg(feature = "ebv")]
pub mod netcdfcf;
#[cfg(feature = "nfdi")]
pub mod pangaea;
