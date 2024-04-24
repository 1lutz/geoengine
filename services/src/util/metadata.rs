use crate::util::Result;
use gdal::vector::LayerAccess;
use geoengine_datatypes::collections::VectorDataType;
use geoengine_operators::util::gdal::gdal_open_dataset;
use std::{
    collections::{hash_map::Entry, HashMap},
    path::Path,
    sync::{Arc, Mutex, OnceLock},
};

#[derive(Clone)]
pub struct PartialVectorResultDescriptor {
    pub data_type: VectorDataType,
}

static DATASET_METADATA: OnceLock<Arc<Mutex<HashMap<String, PartialVectorResultDescriptor>>>> =
    OnceLock::new();

// TODO: change to `LazyLock' once stable
fn init_dataset_metadata() -> Arc<Mutex<HashMap<String, PartialVectorResultDescriptor>>> {
    Arc::new(Mutex::new(HashMap::new()))
}

pub fn fetch_partial_metadata(
    cache_key: String,
    full_path: &Path,
) -> Result<PartialVectorResultDescriptor> {
    let data = Arc::clone(DATASET_METADATA.get_or_init(init_dataset_metadata));
    let mut data = data.lock().unwrap();

    match data.entry(cache_key) {
        Entry::Occupied(o) => {
            println!("Found meta of {}", o.key());
            Ok(o.get().clone())
        },
        Entry::Vacant(v) => {
            println!("Load meta of {}", full_path.display());
            let dataset = gdal_open_dataset(full_path)?;
            let layer = dataset.layer(0)?;
            let descriptor = PartialVectorResultDescriptor {
                data_type: VectorDataType::try_from_ogr_type_code(
                    layer.defn().geom_fields().next().unwrap().field_type(),
                )?,
            };
            println!("Meta downloaded successfully.");
            Ok(v.insert(descriptor).clone())
        }
    }
}
