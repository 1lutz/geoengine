use crate::contexts::GeoEngineDb;
use crate::datasets::listing::{Provenance, ProvenanceOutput};
use crate::error::{Error, Result};
use crate::layers::external::{DataProvider, DataProviderDefinition};
use crate::layers::layer::{
    CollectionItem, Layer, LayerCollection, LayerCollectionListOptions, LayerCollectionListing,
    LayerListing, ProviderLayerCollectionId, ProviderLayerId,
};
use crate::layers::listing::{
    LayerCollectionId, LayerCollectionProvider, ProviderCapabilities, SearchCapabilities,
};
use crate::util::parsing::deserialize_base_url;
use crate::workflows::workflow::Workflow;
use async_trait::async_trait;
use gdal::Dataset;
use geoengine_datatypes::collections::VectorDataType;
use geoengine_datatypes::dataset::{DataId, DataProviderId, LayerId};
use geoengine_datatypes::hashmap;
use geoengine_datatypes::primitives::{
    AxisAlignedRectangle, BoundingBox2D, CacheTtlSeconds, ContinuousMeasurement, Coordinate2D,
    FeatureDataType, Measurement, RasterQueryRectangle, SpatialPartition2D, TimeInstance,
    TimeInterval, VectorQueryRectangle,
};
use geoengine_datatypes::raster::RasterDataType;
use geoengine_datatypes::spatial_reference::SpatialReference;
use geoengine_operators::engine::{
    MetaData, MetaDataProvider, RasterBandDescriptors, RasterOperator, RasterResultDescriptor,
    ResultDescriptor, TypedOperator, VectorColumnInfo, VectorOperator, VectorResultDescriptor,
};
use geoengine_operators::mock::MockDatasetDataSourceLoadingInfo;
use geoengine_operators::source::{
    FileNotFoundHandling, GdalDatasetParameters, GdalLoadingInfo, GdalLoadingInfoTemporalSlice,
    GdalMetaDataList, GdalSource, GdalSourceParameters, OgrSource, OgrSourceColumnSpec,
    OgrSourceDataset, OgrSourceDatasetTimeType, OgrSourceDurationSpec, OgrSourceErrorSpec,
    OgrSourceParameters, OgrSourceTimeFormat,
};
use geoengine_operators::util::gdal::gdal_open_dataset;
use geoengine_operators::util::TemporaryGdalThreadLocalConfigOptions;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use snafu::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::OnceLock;
use url::Url;

static IS_FILETYPE_RASTER: OnceLock<HashMap<&'static str, bool>> = OnceLock::new();

// TODO: change to `LazyLock' once stable
fn init_is_filetype_raster() -> HashMap<&'static str, bool> {
    //name:is_raster
    hashmap! {
        "GeoTIFF" => true,
        "GeoJSON" => false
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EdrDataProviderDefinition {
    pub name: String,
    pub description: String,
    pub priority: Option<i16>,
    pub id: DataProviderId,
    #[serde(deserialize_with = "deserialize_base_url")]
    pub base_url: Url,
    pub vector_spec: Option<EdrVectorSpec>,
    #[serde(default)]
    pub cache_ttl: CacheTtlSeconds,
    #[serde(default)]
    /// List of vertical reference systems with a discrete scale
    pub discrete_vrs: Vec<String>,
    pub provenance: Option<Vec<Provenance>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EdrVectorSpec {
    pub x: String,
    pub y: Option<String>,
    pub start_time: String,
    pub end_time: Option<String>,
}

#[async_trait]
impl<D: GeoEngineDb> DataProviderDefinition<D> for EdrDataProviderDefinition {
    async fn initialize(self: Box<Self>, _db: D) -> Result<Box<dyn DataProvider>> {
        Ok(Box::new(EdrDataProvider {
            id: self.id,
            name: self.name,
            description: self.description,
            base_url: self.base_url,
            vector_spec: self.vector_spec,
            client: Client::new(),
            cache_ttl: self.cache_ttl,
            discrete_vrs: self.discrete_vrs,
            provenance: self.provenance,
        }))
    }

    fn type_name(&self) -> &'static str {
        "Environmental Data Retrieval"
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn id(&self) -> DataProviderId {
        self.id
    }

    fn priority(&self) -> i16 {
        self.priority.unwrap_or(0)
    }
}

#[derive(Debug)]
pub struct EdrDataProvider {
    id: DataProviderId,
    name: String,
    description: String,
    base_url: Url,
    vector_spec: Option<EdrVectorSpec>,
    client: Client,
    cache_ttl: CacheTtlSeconds,
    /// List of vertical reference systems with a discrete scale
    discrete_vrs: Vec<String>,
    provenance: Option<Vec<Provenance>>,
}

#[async_trait]
impl DataProvider for EdrDataProvider {
    async fn provenance(&self, id: &DataId) -> Result<ProvenanceOutput> {
        Ok(ProvenanceOutput {
            data: id.clone(),
            provenance: self.provenance.clone(),
        })
    }
}

impl EdrDataProvider {
    async fn load_collection_by_name(
        &self,
        collection_name: &str,
    ) -> Result<EdrCollectionMetaData> {
        self.client
            .get(
                self.base_url
                    .join(&format!("collections/{collection_name}?f=json"))?,
            )
            .send()
            .await?
            .json()
            .await
            //.map_err(|err| Error::EdrInvalidMetadataFormat)
            .context(crate::error::EdrInvalidMetadataFormat)
    }

    async fn load_collection_by_dataid(
        &self,
        id: &geoengine_datatypes::dataset::DataId,
    ) -> Result<(EdrCollectionId, EdrCollectionMetaData), geoengine_operators::error::Error> {
        let layer_id = id
            .external()
            .ok_or(Error::InvalidDataId)
            .map_err(|e| geoengine_operators::error::Error::LoadingInfo {
                source: Box::new(e),
            })?
            .layer_id;
        let edr_id: EdrCollectionId = EdrCollectionId::from_str(&layer_id.0).map_err(|e| {
            geoengine_operators::error::Error::LoadingInfo {
                source: Box::new(e),
            }
        })?;
        let collection_name = edr_id.get_collection_id().map_err(|e| {
            geoengine_operators::error::Error::LoadingInfo {
                source: Box::new(e),
            }
        })?;
        let collection_meta: EdrCollectionMetaData = self
            .load_collection_by_name(collection_name)
            .await
            .map_err(|e| geoengine_operators::error::Error::LoadingInfo {
                source: Box::new(e),
            })?;
        Ok((edr_id, collection_meta))
    }

    async fn get_root_collection(
        &self,
        collection_id: &LayerCollectionId,
        options: &LayerCollectionListOptions,
    ) -> Result<LayerCollection> {
        let collections: EdrCollectionsMetaData = self
            .client
            .get(self.base_url.join("collections?f=json")?)
            .send()
            .await?
            .json()
            .await
            .context(crate::error::EdrInvalidMetadataFormat)?;

        let items = collections
            .collections
            .into_iter()
            .filter(|collection| {
                collection.data_queries.cube.is_some() && collection.extent.spatial.is_some()
            })
            .skip(options.offset as usize)
            .take(options.limit as usize)
            .map(|collection| {
                if collection.is_raster_file()? || collection.extent.vertical.is_some() {
                    Ok(CollectionItem::Collection(LayerCollectionListing {
                        id: ProviderLayerCollectionId {
                            provider_id: self.id,
                            collection_id: EdrCollectionId::Collection {
                                collection: collection.id.clone(),
                            }
                            .try_into()?,
                        },
                        name: collection.title.unwrap_or(collection.id),
                        description: collection.description.unwrap_or_default(),
                        properties: vec![],
                    }))
                } else {
                    Ok(CollectionItem::Layer(LayerListing {
                        id: ProviderLayerId {
                            provider_id: self.id,
                            layer_id: EdrCollectionId::Collection {
                                collection: collection.id.clone(),
                            }
                            .try_into()?,
                        },
                        name: collection.title.unwrap_or(collection.id),
                        description: collection.description.unwrap_or_default(),
                        properties: vec![],
                    }))
                }
            })
            .collect::<Result<Vec<CollectionItem>>>()?;

        Ok(LayerCollection {
            id: ProviderLayerCollectionId {
                provider_id: self.id,
                collection_id: collection_id.clone(),
            },
            name: "EDR".to_owned(),
            description: "Environmental Data Retrieval".to_owned(),
            items,
            entry_label: None,
            properties: vec![],
        })
    }

    fn get_raster_parameter_collection(
        &self,
        collection_id: &LayerCollectionId,
        collection_meta: EdrCollectionMetaData,
        options: &LayerCollectionListOptions,
    ) -> Result<LayerCollection> {
        let items = collection_meta
            .parameter_names
            .into_keys()
            .skip(options.offset as usize)
            .take(options.limit as usize)
            .map(|parameter_name| {
                if collection_meta.extent.vertical.is_some() {
                    Ok(CollectionItem::Collection(LayerCollectionListing {
                        id: ProviderLayerCollectionId {
                            provider_id: self.id,
                            collection_id: EdrCollectionId::ParameterOrHeight {
                                collection: collection_meta.id.clone(),
                                parameter: parameter_name.clone(),
                            }
                            .try_into()?,
                        },
                        name: parameter_name,
                        description: String::new(),
                        properties: vec![],
                    }))
                } else {
                    Ok(CollectionItem::Layer(LayerListing {
                        id: ProviderLayerId {
                            provider_id: self.id,
                            layer_id: EdrCollectionId::ParameterOrHeight {
                                collection: collection_meta.id.clone(),
                                parameter: parameter_name.clone(),
                            }
                            .try_into()?,
                        },
                        name: parameter_name,
                        description: String::new(),
                        properties: vec![],
                    }))
                }
            })
            .collect::<Result<Vec<CollectionItem>>>()?;

        Ok(LayerCollection {
            id: ProviderLayerCollectionId {
                provider_id: self.id,
                collection_id: collection_id.clone(),
            },
            name: collection_meta.id.clone(),
            description: format!("Parameters of {}", collection_meta.id),
            items,
            entry_label: None,
            properties: vec![],
        })
    }

    fn get_vector_height_collection(
        &self,
        collection_id: &LayerCollectionId,
        collection_meta: EdrCollectionMetaData,
        options: &LayerCollectionListOptions,
    ) -> Result<LayerCollection> {
        let items = collection_meta
            .extent
            .vertical
            .expect("checked before")
            .values
            .into_iter()
            .skip(options.offset as usize)
            .take(options.limit as usize)
            .map(|height| {
                Ok(CollectionItem::Layer(LayerListing {
                    id: ProviderLayerId {
                        provider_id: self.id,
                        layer_id: EdrCollectionId::ParameterOrHeight {
                            collection: collection_meta.id.clone(),
                            parameter: height.clone(),
                        }
                        .try_into()?,
                    },
                    name: height,
                    description: String::new(),
                    properties: vec![],
                }))
            })
            .collect::<Result<Vec<CollectionItem>>>()?;

        Ok(LayerCollection {
            id: ProviderLayerCollectionId {
                provider_id: self.id,
                collection_id: collection_id.clone(),
            },
            name: collection_meta.id.clone(),
            description: format!("Height selection of {}", collection_meta.id),
            items,
            entry_label: None,
            properties: vec![],
        })
    }

    fn get_raster_height_collection(
        &self,
        collection_id: &LayerCollectionId,
        collection_meta: EdrCollectionMetaData,
        parameter: &str,
        options: &LayerCollectionListOptions,
    ) -> Result<LayerCollection> {
        let items = collection_meta
            .extent
            .vertical
            .expect("checked before")
            .values
            .into_iter()
            .skip(options.offset as usize)
            .take(options.limit as usize)
            .map(|height| {
                Ok(CollectionItem::Layer(LayerListing {
                    id: ProviderLayerId {
                        provider_id: self.id,
                        layer_id: EdrCollectionId::ParameterAndHeight {
                            collection: collection_meta.id.clone(),
                            parameter: parameter.to_string(),
                            height: height.clone(),
                        }
                        .try_into()?,
                    },
                    name: height,
                    description: String::new(),
                    properties: vec![],
                }))
            })
            .collect::<Result<Vec<CollectionItem>>>()?;

        Ok(LayerCollection {
            id: ProviderLayerCollectionId {
                provider_id: self.id,
                collection_id: collection_id.clone(),
            },
            name: collection_meta.id.clone(),
            description: format!("Height selection of {}", collection_meta.id),
            items,
            entry_label: None,
            properties: vec![],
        })
    }
}

#[derive(Deserialize)]
struct EdrCollectionsMetaData {
    collections: Vec<EdrCollectionMetaData>,
}

#[derive(Deserialize)]
struct EdrCollectionMetaData {
    id: String,
    title: Option<String>,
    description: Option<String>,
    keywords: Option<Vec<String>>,
    extent: EdrExtents,
    //for paging keys need to be returned in same order every time
    parameter_names: BTreeMap<String, EdrParameter>,
    output_formats: Vec<String>,
    data_queries: EdrDataQueries,
}

#[derive(Deserialize)]
struct EdrDataQueries {
    cube: Option<serde_json::Value>,
}

impl EdrCollectionMetaData {
    fn get_time_interval(&self) -> Result<TimeInterval, geoengine_operators::error::Error> {
        let temporal_extent = self.extent.temporal.as_ref().ok_or_else(|| {
            geoengine_operators::error::Error::DatasetMetaData {
                source: Box::new(EdrProviderError::MissingTemporalExtent),
            }
        })?;

        time_interval_from_strings(
            &temporal_extent.interval[0][0],
            &temporal_extent.interval[0][1],
        )
    }

    fn get_bounding_box(&self) -> Result<BoundingBox2D, geoengine_operators::error::Error> {
        let spatial_extent = self.extent.spatial.as_ref().ok_or_else(|| {
            geoengine_operators::error::Error::DatasetMetaData {
                source: Box::new(EdrProviderError::MissingSpatialExtent),
            }
        })?;

        Ok(BoundingBox2D::new_unchecked(
            Coordinate2D::new(spatial_extent.bbox[0][0], spatial_extent.bbox[0][1]),
            Coordinate2D::new(spatial_extent.bbox[0][2], spatial_extent.bbox[0][3]),
        ))
    }

    fn select_output_format(&self) -> Result<String, geoengine_operators::error::Error> {
        for format in &self.output_formats {
            if IS_FILETYPE_RASTER
                .get_or_init(init_is_filetype_raster)
                .contains_key(format.as_str())
            {
                return Ok(format.to_string());
            }
        }
        Err(geoengine_operators::error::Error::DatasetMetaData {
            source: Box::new(EdrProviderError::NoSupportedOutputFormat),
        })
    }

    fn is_raster_file(&self) -> Result<bool, geoengine_operators::error::Error> {
        Ok(*IS_FILETYPE_RASTER
            .get_or_init(init_is_filetype_raster)
            .get(&self.select_output_format()?.as_str())
            .expect("can only return values in map"))
    }

    fn get_vector_download_url(
        &self,
        base_url: &Url,
        height: &str,
        discrete_vrs: &[String],
    ) -> Result<(String, String), geoengine_operators::error::Error> {
        let z = if height == "default" {
            String::new()
        } else if self.extent.has_discrete_vertical_axis(discrete_vrs) {
            format!("&z={height}")
        } else {
            format!("&z={height}%2F{height}")
        };
        let download_url = format!(
            "/vsicurl_streaming/{}collections/{}/cube?f={}{}",
            base_url,
            self.id,
            self.select_output_format()?,
            z
        );
        let mut layer_name = format!("cube?f={}{}", self.select_output_format()?, z);
        if let Some(last_dot_pos) = layer_name.rfind('.') {
            layer_name = layer_name[0..last_dot_pos].to_string();
        }
        Ok((download_url, layer_name))
    }

    fn get_raster_download_url(
        &self,
        base_url: &Url,
        parameter_name: &str,
        height: &str,
        time: &str,
        discrete_vrs: &[String],
    ) -> Result<String, geoengine_operators::error::Error> {
        let spatial_extent = self.extent.spatial.as_ref().ok_or_else(|| {
            geoengine_operators::error::Error::DatasetMetaData {
                source: Box::new(EdrProviderError::MissingSpatialExtent),
            }
        })?;
        let z = if height == "default" {
            String::new()
        } else if self.extent.has_discrete_vertical_axis(discrete_vrs) {
            format!("&z={height}")
        } else {
            format!("&z={height}%2F{height}")
        };
        Ok(format!(
            "/vsicurl_streaming/{}collections/{}/cube?bbox={},{},{},{}{}&datetime={}%2F{}&f={}&parameter-name={}",
            base_url,
            self.id,
            spatial_extent.bbox[0][0],
            spatial_extent.bbox[0][1],
            spatial_extent.bbox[0][2],
            spatial_extent.bbox[0][3],
            z,
            time,
            time,
            self.select_output_format()?,
            parameter_name
        ))
    }

    fn determine_vector_data_type(
        &self,
        download_url: &str,
        layer_name: &str,
    ) -> Result<VectorDataType, geoengine_operators::error::Error> {
        let mut vector_data_type = None;

        if let Some(keywords) = &self.keywords {
            for keyword in keywords {
                match keyword.as_str() {
                    "Point" | "MultiPoint" => vector_data_type = Some(VectorDataType::MultiPoint),
                    "LineString" | "MultiLineString" => {
                        vector_data_type = Some(VectorDataType::MultiLineString)
                    }
                    "Polygon" | "MultiPolygon" => {
                        vector_data_type = Some(VectorDataType::MultiPolygon)
                    }
                    _ => {}
                }
                if vector_data_type.is_some() {
                    break;
                }
            }
        }
        if vector_data_type.is_none() {
            println!("Open vector gdal for type detect: {download_url}");
            /*let dataset = gdal_open_dataset(std::path::Path::new(download_url))?;
            let layer = dataset.layer_by_name(layer_name)?;
            vector_data_type =
                Some(crate::api::handlers::datasets::detect_vector_geometry(&layer).data_type);*/
            vector_data_type = Some(VectorDataType::MultiPoint);
        }

        Ok(vector_data_type.unwrap())
    }

    fn get_vector_result_descriptor(
        &self,
        download_url: &str,
        layer_name: &str,
    ) -> Result<VectorResultDescriptor, geoengine_operators::error::Error> {
        let column_map: HashMap<String, VectorColumnInfo> = self
            .parameter_names
            .iter()
            .map(|(parameter_name, parameter_metadata)| {
                let data_type = if let Some(data_type) = parameter_metadata.data_type.as_ref() {
                    data_type.as_str().to_uppercase()
                } else {
                    "FLOAT".to_string()
                };
                match data_type.as_str() {
                    "STRING" => (
                        parameter_name.to_string(),
                        VectorColumnInfo {
                            data_type: FeatureDataType::Text,
                            measurement: Measurement::Unitless,
                        },
                    ),
                    "INTEGER" => (
                        parameter_name.to_string(),
                        VectorColumnInfo {
                            data_type: FeatureDataType::Int,
                            measurement: Measurement::Continuous(ContinuousMeasurement {
                                measurement: parameter_metadata.observed_property.label.clone(),
                                unit: parameter_metadata.unit.as_ref().map(|x| x.symbol.value()),
                            }),
                        },
                    ),
                    _ => (
                        parameter_name.to_string(),
                        VectorColumnInfo {
                            data_type: FeatureDataType::Float,
                            measurement: Measurement::Continuous(ContinuousMeasurement {
                                measurement: parameter_metadata.observed_property.label.clone(),
                                unit: parameter_metadata.unit.as_ref().map(|x| x.symbol.value()),
                            }),
                        },
                    ),
                }
            })
            .collect();

        Ok(VectorResultDescriptor {
            spatial_reference: SpatialReference::epsg_4326().into(),
            data_type: self.determine_vector_data_type(download_url, layer_name)?,
            columns: column_map,
            time: Some(self.get_time_interval()?),
            bbox: Some(self.get_bounding_box()?),
        })
    }

    fn get_column_spec(&self, vector_spec: EdrVectorSpec) -> OgrSourceColumnSpec {
        let mut int = vec![];
        let mut float = vec![];
        let mut text = vec![];
        let bool = vec![];
        let datetime = vec![];

        for (parameter_name, parameter_metadata) in &self.parameter_names {
            let data_type = if let Some(data_type) = parameter_metadata.data_type.as_ref() {
                data_type.as_str().to_uppercase()
            } else {
                "FLOAT".to_string()
            };
            match data_type.as_str() {
                "STRING" => {
                    text.push(parameter_name.clone());
                }
                "INTEGER" => {
                    int.push(parameter_name.clone());
                }
                _ => {
                    float.push(parameter_name.clone());
                }
            }
        }
        OgrSourceColumnSpec {
            format_specifics: None,
            x: vector_spec.x,
            y: vector_spec.y,
            int,
            float,
            text,
            bool,
            datetime,
            rename: None,
        }
    }

    fn get_ogr_source_ds(
        &self,
        download_url: &str,
        layer_name: &str,
        vector_spec: EdrVectorSpec,
        cache_ttl: CacheTtlSeconds,
    ) -> OgrSourceDataset {
        let time = if vector_spec.end_time.is_some() {
            OgrSourceDatasetTimeType::StartEnd {
                start_field: vector_spec.start_time.clone(),
                start_format: OgrSourceTimeFormat::Auto,
                end_field: vector_spec.end_time.clone().unwrap(),
                end_format: OgrSourceTimeFormat::Auto,
            }
        } else {
            OgrSourceDatasetTimeType::Start {
                start_field: vector_spec.start_time.clone(),
                start_format: OgrSourceTimeFormat::Auto,
                duration: OgrSourceDurationSpec::Zero,
            }
        };

        OgrSourceDataset {
            file_name: PathBuf::from(download_url),
            layer_name: layer_name.to_string(),
            data_type: self
                .determine_vector_data_type(download_url, layer_name)
                .ok(),
            time,
            default_geometry: None,
            columns: Some(self.get_column_spec(vector_spec)),
            force_ogr_time_filter: false,
            force_ogr_spatial_filter: false,
            on_error: OgrSourceErrorSpec::Abort,
            sql_query: None,
            attribute_query: None,
            cache_ttl,
        }
    }

    fn get_ogr_metadata(
        &self,
        base_url: &Url,
        height: &str,
        vector_spec: EdrVectorSpec,
        cache_ttl: CacheTtlSeconds,
        discrete_vrs: &[String],
    ) -> Result<EdrMetaData<OgrSourceDataset, VectorResultDescriptor, VectorQueryRectangle>> {
        let (download_url, layer_name) =
            self.get_vector_download_url(base_url, height, discrete_vrs)?;
        let omd = self.get_ogr_source_ds(&download_url, &layer_name, vector_spec, cache_ttl);

        Ok(EdrMetaData {
            loading_info: omd,
            result_descriptor: self.get_vector_result_descriptor(&download_url, &layer_name)?,
            template_url: download_url,
            phantom: Default::default(),
        })
    }

    fn get_raster_result_descriptor(
        &self,
    ) -> Result<RasterResultDescriptor, geoengine_operators::error::Error> {
        let bbox = self.get_bounding_box()?;
        let bbox = SpatialPartition2D::new_unchecked(bbox.upper_left(), bbox.lower_right());

        Ok(RasterResultDescriptor {
            data_type: RasterDataType::U8,
            spatial_reference: SpatialReference::epsg_4326().into(),
            time: Some(self.get_time_interval()?),
            bbox: Some(bbox),
            resolution: None,
            bands: RasterBandDescriptors::new_single_band(),
        })
    }

    fn get_gdal_loading_info_temporal_slice(
        &self,
        provider: &EdrDataProvider,
        parameter: &str,
        height: &str,
        data_time: TimeInterval,
        current_time: &str,
        dataset: &Dataset,
    ) -> Result<GdalLoadingInfoTemporalSlice, geoengine_operators::error::Error> {
        let rasterband = &dataset.rasterband(1)?;

        Ok(GdalLoadingInfoTemporalSlice {
            time: data_time,
            params: Some(GdalDatasetParameters {
                file_path: self
                    .get_raster_download_url(
                        &provider.base_url,
                        parameter,
                        height,
                        current_time,
                        &provider.discrete_vrs,
                    )?
                    .into(),
                rasterband_channel: 1,
                geo_transform: dataset
                    .geo_transform()
                    .context(crate::error::Gdal)
                    .map_err(|e| geoengine_operators::error::Error::LoadingInfo {
                        source: Box::new(e),
                    })?
                    .into(),
                width: rasterband.x_size(),
                height: rasterband.y_size(),
                file_not_found_handling: FileNotFoundHandling::NoData,
                no_data_value: None,
                properties_mapping: None,
                gdal_open_options: None,
                gdal_config_options: Some(vec![(
                    "GTIFF_HONOUR_NEGATIVE_SCALEY".to_string(),
                    "YES".to_string(),
                )]),
                allow_alphaband_as_mask: false,
                retry: None,
            }),
            cache_ttl: provider.cache_ttl,
        })
    }
}

#[derive(Deserialize)]
struct EdrExtents {
    spatial: Option<EdrSpatialExtent>,
    vertical: Option<EdrVerticalExtent>,
    temporal: Option<EdrTemporalExtent>,
}

impl EdrExtents {
    fn has_discrete_vertical_axis(&self, discrete_vrs: &[String]) -> bool {
        self.vertical
            .as_ref()
            .map_or(false, |val| discrete_vrs.contains(&val.vrs))
    }
}

#[derive(Deserialize)]
struct EdrSpatialExtent {
    bbox: Vec<Vec<f64>>,
}

#[derive(Deserialize)]
struct EdrVerticalExtent {
    values: Vec<String>,
    vrs: String,
}

#[derive(Deserialize, Clone)]
struct EdrTemporalExtent {
    interval: Vec<Vec<String>>,
    values: Vec<String>,
}

#[derive(Deserialize)]
struct EdrParameter {
    #[serde(rename = "data-type")]
    data_type: Option<String>,
    unit: Option<EdrUnit>,
    #[serde(rename = "observedProperty")]
    observed_property: ObservedProperty,
}

#[derive(Deserialize)]
struct EdrUnit {
    symbol: EdrSymbol,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum EdrSymbol {
    Scalar(String),
    Object { value: String },
}

impl EdrSymbol {
    fn value(&self) -> String {
        match self {
            EdrSymbol::Scalar(value) => value,
            EdrSymbol::Object { value } => value,
        }
        .clone()
    }
}

#[derive(Deserialize)]
struct ObservedProperty {
    label: String,
}

enum EdrCollectionId {
    Collections,
    Collection {
        collection: String,
    },
    ParameterOrHeight {
        collection: String,
        parameter: String,
    },
    ParameterAndHeight {
        collection: String,
        parameter: String,
        height: String,
    },
}

impl EdrCollectionId {
    fn get_collection_id(&self) -> Result<&String> {
        match self {
            EdrCollectionId::Collections => Err(Error::InvalidLayerId),
            EdrCollectionId::Collection { collection }
            | EdrCollectionId::ParameterOrHeight { collection, .. }
            | EdrCollectionId::ParameterAndHeight { collection, .. } => Ok(collection),
        }
    }
}

impl FromStr for EdrCollectionId {
    type Err = Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        // Collection ids use ampersands as separators because some collection names
        // contain slashes.
        let split = s.split('!').collect::<Vec<_>>();

        Ok(match *split.as_slice() {
            ["collections"] => EdrCollectionId::Collections,
            ["collections", collection] => EdrCollectionId::Collection {
                collection: collection.to_string(),
            },
            ["collections", collection, parameter] => EdrCollectionId::ParameterOrHeight {
                collection: collection.to_string(),
                parameter: parameter.to_string(),
            },
            ["collections", collection, parameter, height] => EdrCollectionId::ParameterAndHeight {
                collection: collection.to_string(),
                parameter: parameter.to_string(),
                height: height.to_string(),
            },
            _ => return Err(Error::InvalidLayerCollectionId),
        })
    }
}

impl TryFrom<EdrCollectionId> for LayerCollectionId {
    type Error = Error;

    fn try_from(value: EdrCollectionId) -> std::result::Result<Self, Self::Error> {
        let s = match value {
            EdrCollectionId::Collections => "collections".to_string(),
            EdrCollectionId::Collection { collection } => format!("collections!{collection}"),
            EdrCollectionId::ParameterOrHeight {
                collection,
                parameter,
            } => format!("collections!{collection}!{parameter}"),
            EdrCollectionId::ParameterAndHeight { .. } => {
                return Err(Error::InvalidLayerCollectionId)
            }
        };

        Ok(LayerCollectionId(s))
    }
}

impl TryFrom<EdrCollectionId> for LayerId {
    type Error = Error;

    fn try_from(value: EdrCollectionId) -> std::result::Result<Self, Self::Error> {
        let s = match value {
            EdrCollectionId::Collections => return Err(Error::InvalidLayerId),
            EdrCollectionId::Collection { collection } => format!("collections!{collection}"),
            EdrCollectionId::ParameterOrHeight {
                collection,
                parameter,
            } => format!("collections!{collection}!{parameter}"),
            EdrCollectionId::ParameterAndHeight {
                collection,
                parameter,
                height,
            } => format!("collections!{collection}!{parameter}!{height}"),
        };

        Ok(LayerId(s))
    }
}

trait EdrLinkGenerator<Q>
where
    Q: Debug + Clone + Send + Sync + 'static,
{
    fn new_link(
        &self,
        template_url: &String,
        query: Q,
    ) -> std::result::Result<Self, geoengine_operators::error::Error>
    where
        Self: Sized;
}

impl EdrLinkGenerator<VectorQueryRectangle> for OgrSourceDataset {
    fn new_link(
        &self,
        template_url: &String,
        query: VectorQueryRectangle,
    ) -> std::result::Result<Self, geoengine_operators::error::Error>
    where
        Self: Sized,
    {
        let start = query
            .time_interval
            .start()
            .as_datetime_string()
            .replace("+", "%2B");
        let end = query
            .time_interval
            .end()
            .as_datetime_string()
            .replace("+", "%2B");
        let query_rectangle = format!(
            "&bbox={},{},{},{}&datetime={}%2F{}",
            query.spatial_bounds.lower_left().x,
            query.spatial_bounds.lower_left().y,
            query.spatial_bounds.upper_right().x,
            query.spatial_bounds.upper_left().y,
            start,
            end
        );
        let new_file_name = PathBuf::from(template_url.to_owned() + &query_rectangle);
        let mut new_layer_name = self.layer_name.clone() + &query_rectangle;
        if let Some(last_dot_pos) = new_layer_name.rfind('.') {
            new_layer_name = new_layer_name[0..last_dot_pos].to_string();
        }

        Ok(Self {
            file_name: new_file_name,
            layer_name: new_layer_name,
            data_type: self.data_type,
            time: self.time.clone(),
            default_geometry: self.default_geometry.clone(),
            columns: self.columns.clone(),
            force_ogr_time_filter: self.force_ogr_time_filter,
            force_ogr_spatial_filter: self.force_ogr_spatial_filter,
            on_error: self.on_error,
            sql_query: self.sql_query.clone(),
            attribute_query: self.attribute_query.clone(),
            cache_ttl: self.cache_ttl,
        })
    }
}

#[derive(Debug, Clone)]
struct EdrMetaData<L, R, Q>
where
    L: Debug + Clone + Send + Sync + EdrLinkGenerator<Q> + 'static,
    R: Debug + Send + Sync + 'static + ResultDescriptor,
    Q: Debug + Clone + Send + Sync + 'static,
{
    pub loading_info: L,
    pub result_descriptor: R,
    pub template_url: String,
    pub phantom: PhantomData<Q>,
}

#[async_trait::async_trait]
impl<L, R, Q> MetaData<L, R, Q> for EdrMetaData<L, R, Q>
where
    L: Debug + Clone + Send + Sync + EdrLinkGenerator<Q> + 'static,
    R: Debug + Send + Sync + 'static + ResultDescriptor,
    Q: Debug + Clone + Send + Sync + 'static,
{
    async fn loading_info(&self, query: Q) -> geoengine_operators::util::Result<L> {
        self.loading_info.new_link(&self.template_url, query)
    }

    async fn result_descriptor(&self) -> geoengine_operators::util::Result<R> {
        Ok(self.result_descriptor.clone())
    }

    fn box_clone(&self) -> Box<dyn MetaData<L, R, Q>> {
        Box::new(self.clone())
    }
}

#[async_trait]
impl LayerCollectionProvider for EdrDataProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            listing: true,
            search: SearchCapabilities::none(),
        }
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn load_layer_collection(
        &self,
        collection_id: &LayerCollectionId,
        options: LayerCollectionListOptions,
    ) -> Result<LayerCollection> {
        let edr_id: EdrCollectionId = EdrCollectionId::from_str(&collection_id.0)
            .map_err(|_e| Error::InvalidLayerCollectionId)?;

        match edr_id {
            EdrCollectionId::Collections => self.get_root_collection(collection_id, &options).await,
            EdrCollectionId::Collection { collection } => {
                let collection_meta = self.load_collection_by_name(&collection).await?;

                if collection_meta.is_raster_file()? {
                    // The collection is of type raster. A layer can only contain one parameter
                    // of a raster dataset at a time, so let the user choose one.
                    self.get_raster_parameter_collection(collection_id, collection_meta, &options)
                } else if collection_meta.extent.vertical.is_some() {
                    // The collection is of type vector and data is provided for multiple heights.
                    // The user needs to be able to select the height he wants to see. It is not
                    // needed to select a parameter, because for vector datasets all parameters
                    // can be loaded simultaneously.
                    self.get_vector_height_collection(collection_id, collection_meta, &options)
                } else {
                    // The collection is of type vector and there is only data for a single height.
                    // No height or parameter needs to be selected by the user. Therefore the name
                    // of the collection already identifies a layer sufficiently.
                    Err(Error::InvalidLayerCollectionId)
                }
            }
            EdrCollectionId::ParameterOrHeight {
                collection,
                parameter,
            } => {
                let collection_meta = self.load_collection_by_name(&collection).await?;

                if !collection_meta.is_raster_file()? || collection_meta.extent.vertical.is_none() {
                    // When the collection is of type raster, the parameter-name is set by the
                    // parameter field. The height must not be selected when the collection has
                    // no height information.
                    // When the collection is of type vector, the height is already set by the
                    // parameter field. For vectors no parameter-name must be selected.
                    return Err(Error::InvalidLayerCollectionId);
                }
                // If the program gets here, it is a raster collection and it contains multiple
                // heights. The parameter-name was already chosen by the paramter field, but a
                // height must still be selected.
                self.get_raster_height_collection(
                    collection_id,
                    collection_meta,
                    &parameter,
                    &options,
                )
            }
            EdrCollectionId::ParameterAndHeight { .. } => Err(Error::InvalidLayerCollectionId),
        }
    }

    async fn get_root_layer_collection_id(&self) -> Result<LayerCollectionId> {
        EdrCollectionId::Collections.try_into()
    }

    async fn load_layer(&self, id: &LayerId) -> Result<Layer> {
        let edr_id: EdrCollectionId = EdrCollectionId::from_str(&id.0)?;
        let collection_id = edr_id.get_collection_id()?;

        let collection = self.load_collection_by_name(collection_id).await?;

        let operator = if collection.is_raster_file()? {
            TypedOperator::Raster(
                GdalSource {
                    params: GdalSourceParameters {
                        data: geoengine_datatypes::dataset::NamedData::with_system_provider(
                            self.id.to_string(),
                            id.to_string(),
                        ),
                    },
                }
                .boxed(),
            )
        } else {
            TypedOperator::Vector(
                OgrSource {
                    params: OgrSourceParameters {
                        data: geoengine_datatypes::dataset::NamedData::with_system_provider(
                            self.id.to_string(),
                            id.to_string(),
                        ),
                        attribute_projection: None,
                        attribute_filters: None,
                    },
                }
                .boxed(),
            )
        };

        Ok(Layer {
            id: ProviderLayerId {
                provider_id: self.id,
                layer_id: id.clone(),
            },
            name: collection.title.unwrap_or(collection.id),
            description: String::new(),
            workflow: Workflow { operator },
            symbology: None, // TODO
            properties: vec![],
            metadata: HashMap::new(),
        })
    }
}

#[async_trait]
impl
    MetaDataProvider<MockDatasetDataSourceLoadingInfo, VectorResultDescriptor, VectorQueryRectangle>
    for EdrDataProvider
{
    async fn meta_data(
        &self,
        _id: &geoengine_datatypes::dataset::DataId,
    ) -> Result<
        Box<
            dyn MetaData<
                MockDatasetDataSourceLoadingInfo,
                VectorResultDescriptor,
                VectorQueryRectangle,
            >,
        >,
        geoengine_operators::error::Error,
    > {
        Err(geoengine_operators::error::Error::NotYetImplemented)
    }
}

#[async_trait]
impl MetaDataProvider<OgrSourceDataset, VectorResultDescriptor, VectorQueryRectangle>
    for EdrDataProvider
{
    async fn meta_data(
        &self,
        id: &geoengine_datatypes::dataset::DataId,
    ) -> Result<
        Box<dyn MetaData<OgrSourceDataset, VectorResultDescriptor, VectorQueryRectangle>>,
        geoengine_operators::error::Error,
    > {
        let vector_spec = self.vector_spec.clone().ok_or_else(|| {
            geoengine_operators::error::Error::DatasetMetaData {
                source: Box::new(EdrProviderError::NoVectorSpecConfigured),
            }
        })?;
        let (edr_id, collection) = self.load_collection_by_dataid(id).await?;

        let height = match edr_id {
            EdrCollectionId::Collection { .. } => "default".to_string(),
            EdrCollectionId::ParameterOrHeight { parameter, .. } => parameter,
            _ => unreachable!(),
        };

        let smd = collection
            .get_ogr_metadata(
                &self.base_url,
                &height,
                vector_spec,
                self.cache_ttl,
                &self.discrete_vrs,
            )
            .map_err(|e| geoengine_operators::error::Error::LoadingInfo {
                source: Box::new(e),
            })?;

        Ok(Box::new(smd))
    }
}

#[async_trait]
impl MetaDataProvider<GdalLoadingInfo, RasterResultDescriptor, RasterQueryRectangle>
    for EdrDataProvider
{
    async fn meta_data(
        &self,
        id: &geoengine_datatypes::dataset::DataId,
    ) -> Result<
        Box<dyn MetaData<GdalLoadingInfo, RasterResultDescriptor, RasterQueryRectangle>>,
        geoengine_operators::error::Error,
    > {
        let (edr_id, collection) = self.load_collection_by_dataid(id).await?;

        let (parameter, height) = match edr_id {
            EdrCollectionId::ParameterOrHeight { parameter, .. } => {
                (parameter, "default".to_string())
            }
            EdrCollectionId::ParameterAndHeight {
                parameter, height, ..
            } => (parameter, height),
            _ => unreachable!(),
        };

        let mut params: Vec<GdalLoadingInfoTemporalSlice> = Vec::new();
        // reverts the thread local configs on drop
        let _thread_local_configs = TemporaryGdalThreadLocalConfigOptions::new(&[(
            "GTIFF_HONOUR_NEGATIVE_SCALEY".to_string(),
            "YES".to_string(),
        )])?;

        if let Some(temporal_extent) = collection.extent.temporal.clone() {
            let mut temporal_values_iter = temporal_extent.values.iter();
            let mut previous_start = temporal_values_iter
                .next()
                // TODO: check if this could be unwrapped safely
                .ok_or(
                    geoengine_operators::error::Error::InvalidNumberOfTimeSteps {
                        expected: 1,
                        found: 0,
                    },
                )?;
            let dataset = gdal_open_dataset(
                collection
                    .get_raster_download_url(
                        &self.base_url,
                        &parameter,
                        &height,
                        previous_start,
                        &self.discrete_vrs,
                    )?
                    .as_ref(),
            )?;

            for current_time in temporal_values_iter {
                params.push(collection.get_gdal_loading_info_temporal_slice(
                    self,
                    &parameter,
                    &height,
                    time_interval_from_strings(previous_start, current_time)?,
                    previous_start,
                    &dataset,
                )?);
                previous_start = current_time;
            }
            params.push(collection.get_gdal_loading_info_temporal_slice(
                self,
                &parameter,
                &height,
                time_interval_from_strings(previous_start, &temporal_extent.interval[0][1])?,
                previous_start,
                &dataset,
            )?);
        } else {
            let dummy_time = "2023-06-06T00:00:00Z";
            let dataset = gdal_open_dataset(
                collection
                    .get_raster_download_url(
                        &self.base_url,
                        &parameter,
                        &height,
                        dummy_time,
                        &self.discrete_vrs,
                    )?
                    .as_ref(),
            )?;
            params.push(collection.get_gdal_loading_info_temporal_slice(
                self,
                &parameter,
                &height,
                TimeInterval::default(),
                dummy_time,
                &dataset,
            )?);
        }

        Ok(Box::new(GdalMetaDataList {
            result_descriptor: collection.get_raster_result_descriptor()?,
            params,
        }))
    }
}

// TODO: proper error handling
#[allow(clippy::unnecessary_wraps)]
fn time_interval_from_strings(
    start: &str,
    end: &str,
) -> Result<TimeInterval, geoengine_operators::error::Error> {
    let start = TimeInstance::from_str(start).unwrap_or(TimeInstance::MIN);
    let end = TimeInstance::from_str(end).unwrap_or(TimeInstance::MAX);
    Ok(TimeInterval::new_unchecked(start, end))
}

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
#[snafu(context(suffix(false)))] // disables default `Snafu` suffix
pub enum EdrProviderError {
    MissingVerticalExtent,
    MissingSpatialExtent,
    MissingTemporalExtent,
    NoSupportedOutputFormat,
    NoVectorSpecConfigured,
}

#[cfg(test)]
mod tests {
    use crate::{
        contexts::{PostgresContext, PostgresDb, SessionContext, SimpleApplicationContext},
        ge_context,
    };

    use super::*;
    use futures::StreamExt;
    use geoengine_datatypes::{
        dataset::ExternalDataId,
        primitives::{BandSelection, ColumnSelection, SpatialResolution},
        util::test::TestDefault,
    };
    use geoengine_operators::{
        engine::{
            ChunkByteSize, MockExecutionContext, MockQueryContext, QueryProcessor,
            ResultDescriptor, WorkflowOperatorPath,
        },
        source::GdalDatasetGeoTransform,
    };
    use httptest::{matchers::*, responders::status_code, Expectation, Server};
    use std::{ops::Range, path::PathBuf};
    use tokio_postgres::NoTls;

    const DEMO_PROVIDER_ID: DataProviderId =
        DataProviderId::from_u128(0xdc2d_dc34_b0d9_4ee0_bf3e_414f_01a8_05ad);

    fn test_data_path(file_name: &str) -> PathBuf {
        crate::test_data!(String::from("edr/") + file_name).into()
    }

    async fn create_provider<D: GeoEngineDb>(server: &Server, db: D) -> Box<dyn DataProvider> {
        Box::new(EdrDataProviderDefinition {
            name: "EDR".to_string(),
            description: "Environmental Data Retrieval".to_string(),
            priority: None,
            id: DEMO_PROVIDER_ID,
            base_url: Url::parse(server.url_str("").strip_suffix('/').unwrap()).unwrap(),
            vector_spec: Some(EdrVectorSpec {
                x: "geometry".to_string(),
                y: None,
                start_time: "time".to_string(),
                end_time: None,
            }),
            cache_ttl: Default::default(),
            discrete_vrs: vec!["between-depth".to_string()],
            provenance: None,
        })
        .initialize(db)
        .await
        .unwrap()
    }

    async fn setup_url(
        server: &mut Server,
        url: &str,
        content_type: &str,
        file_name: &str,
        times: Range<usize>,
    ) {
        let path = test_data_path(file_name);
        let body = tokio::fs::read(path).await.unwrap();

        let responder = status_code(200)
            .append_header("content-type", content_type.to_owned())
            .append_header("content-length", body.len())
            .body(body);

        server.expect(
            Expectation::matching(request::method_path("GET", url.to_string()))
                .times(times)
                .respond_with(responder),
        );
    }

    async fn prepare_cube_query(
        server: &mut Server,
        collection_name: &str,
        content_type: &str,
        file_name: &str,
        parameter_name: &str,
    ) {
        let url = format!("/collections/{collection_name}/cube");

        setup_url(server, &url, content_type, file_name, 3..5).await;
        server.expect(
            Expectation::matching(all_of![
                request::method_path("HEAD", url),
                request::query(url_decoded(contains((
                    "parameter-name",
                    format!("{parameter_name}.aux.xml")
                ))))
            ])
            .times(0..2)
            .respond_with(status_code(404)),
        );
    }

    async fn load_layer_collection<D: GeoEngineDb>(
        collection: &LayerCollectionId,
        db: D,
    ) -> Result<LayerCollection> {
        let mut server = Server::run();

        if collection.0 == "collections" {
            setup_url(
                &mut server,
                "/collections",
                "application/json",
                "edr_collections.json",
                0..2,
            )
            .await;
        } else {
            let collection_name = collection.0.split('!').nth(1).unwrap();
            setup_url(
                &mut server,
                &format!("/collections/{collection_name}"),
                "application/json",
                &format!("edr_{collection_name}.json"),
                0..2,
            )
            .await;
        }

        let provider = create_provider(&server, db).await;

        let datasets = provider
            .load_layer_collection(
                collection,
                LayerCollectionListOptions {
                    offset: 0,
                    limit: 20,
                },
            )
            .await?;
        server.verify_and_clear();

        Ok(datasets)
    }

    #[ge_context::test]
    async fn it_loads_root_collection(app_ctx: PostgresContext<NoTls>) -> Result<()> {
        let root_collection_id = LayerCollectionId("collections".to_string());
        let datasets = load_layer_collection(
            &root_collection_id,
            app_ctx.default_session_context().await.unwrap().db(),
        )
        .await?;

        pretty_assertions::assert_eq!(
            datasets,
            LayerCollection {
                id: ProviderLayerCollectionId {
                    provider_id: DEMO_PROVIDER_ID,
                    collection_id: root_collection_id
                },
                name: "EDR".to_owned(),
                description: "Environmental Data Retrieval".to_owned(),
                items: vec![
                    // Note: The dataset GFS_single-level_50 gets filtered out because there is no extent set.
                    // This means that it contains no data.
                    CollectionItem::Collection(LayerCollectionListing {
                        id: ProviderLayerCollectionId {
                            provider_id: DEMO_PROVIDER_ID,
                            collection_id: LayerCollectionId(
                                "collections!GFS_single-level".to_string()
                            )
                        },
                        name: "GFS - Single Level".to_string(),
                        description: String::new(),
                        properties: vec![],
                    }),
                    CollectionItem::Collection(LayerCollectionListing {
                        id: ProviderLayerCollectionId {
                            provider_id: DEMO_PROVIDER_ID,
                            collection_id: LayerCollectionId(
                                "collections!GFS_isobaric".to_string()
                            )
                        },
                        name: "GFS - Isobaric level".to_string(),
                        description: String::new(),
                        properties: vec![],
                    }),
                    CollectionItem::Collection(LayerCollectionListing {
                        id: ProviderLayerCollectionId {
                            provider_id: DEMO_PROVIDER_ID,
                            collection_id: LayerCollectionId(
                                "collections!GFS_between-depth".to_string()
                            )
                        },
                        name: "GFS - Layer between two depths below land surface".to_string(),
                        description: String::new(),
                        properties: vec![],
                    }),
                    CollectionItem::Layer(LayerListing {
                        id: ProviderLayerId {
                            provider_id: DEMO_PROVIDER_ID,
                            layer_id: LayerId("collections!PointsInGermany".to_string())
                        },
                        name: "PointsInGermany".to_string(),
                        description: String::new(),
                        properties: vec![],
                    }),
                    CollectionItem::Collection(LayerCollectionListing {
                        id: ProviderLayerCollectionId {
                            provider_id: DEMO_PROVIDER_ID,
                            collection_id: LayerCollectionId(
                                "collections!PointsInFrance".to_string()
                            )
                        },
                        name: "PointsInFrance".to_string(),
                        description: String::new(),
                        properties: vec![],
                    }),
                ],
                entry_label: None,
                properties: vec![]
            }
        );

        Ok(())
    }

    #[ge_context::test]
    async fn it_loads_raster_parameter_collection(app_ctx: PostgresContext<NoTls>) -> Result<()> {
        let collection_id = LayerCollectionId("collections!GFS_isobaric".to_string());
        let datasets = load_layer_collection(
            &collection_id,
            app_ctx.default_session_context().await.unwrap().db(),
        )
        .await?;

        pretty_assertions::assert_eq!(
            datasets,
            LayerCollection {
                id: ProviderLayerCollectionId {
                    provider_id: DEMO_PROVIDER_ID,
                    collection_id
                },
                name: "GFS_isobaric".to_owned(),
                description: "Parameters of GFS_isobaric".to_owned(),
                items: vec![CollectionItem::Collection(LayerCollectionListing {
                    id: ProviderLayerCollectionId {
                        provider_id: DEMO_PROVIDER_ID,
                        collection_id: LayerCollectionId(
                            "collections!GFS_isobaric!temperature".to_string()
                        )
                    },
                    name: "temperature".to_string(),
                    description: String::new(),
                    properties: vec![],
                })],
                entry_label: None,
                properties: vec![]
            }
        );

        Ok(())
    }

    #[ge_context::test]
    async fn it_loads_vector_height_collection(app_ctx: PostgresContext<NoTls>) -> Result<()> {
        let collection_id = LayerCollectionId("collections!PointsInFrance".to_string());
        let datasets = load_layer_collection(
            &collection_id,
            app_ctx.default_session_context().await.unwrap().db(),
        )
        .await?;

        pretty_assertions::assert_eq!(
            datasets,
            LayerCollection {
                id: ProviderLayerCollectionId {
                    provider_id: DEMO_PROVIDER_ID,
                    collection_id
                },
                name: "PointsInFrance".to_owned(),
                description: "Height selection of PointsInFrance".to_owned(),
                items: vec![
                    CollectionItem::Layer(LayerListing {
                        id: ProviderLayerId {
                            provider_id: DEMO_PROVIDER_ID,
                            layer_id: LayerId("collections!PointsInFrance!0\\10cm".to_string())
                        },
                        name: "0\\10cm".to_string(),
                        description: String::new(),
                        properties: vec![],
                    }),
                    CollectionItem::Layer(LayerListing {
                        id: ProviderLayerId {
                            provider_id: DEMO_PROVIDER_ID,
                            layer_id: LayerId("collections!PointsInFrance!10\\40cm".to_string())
                        },
                        name: "10\\40cm".to_string(),
                        description: String::new(),
                        properties: vec![],
                    })
                ],
                entry_label: None,
                properties: vec![]
            }
        );

        Ok(())
    }

    #[ge_context::test]
    async fn vector_without_height_collection_invalid(app_ctx: PostgresContext<NoTls>) {
        let collection_id = LayerCollectionId("collections!PointsInGermany".to_string());
        let res = load_layer_collection(
            &collection_id,
            app_ctx.default_session_context().await.unwrap().db(),
        )
        .await;

        assert!(res.is_err());
    }

    #[ge_context::test]
    async fn it_loads_raster_height_collection(app_ctx: PostgresContext<NoTls>) -> Result<()> {
        let collection_id = LayerCollectionId("collections!GFS_isobaric!temperature".to_string());
        let datasets = load_layer_collection(
            &collection_id,
            app_ctx.default_session_context().await.unwrap().db(),
        )
        .await?;

        pretty_assertions::assert_eq!(
            datasets,
            LayerCollection {
                id: ProviderLayerCollectionId {
                    provider_id: DEMO_PROVIDER_ID,
                    collection_id
                },
                name: "GFS_isobaric".to_owned(),
                description: "Height selection of GFS_isobaric".to_owned(),
                items: vec![
                    CollectionItem::Layer(LayerListing {
                        id: ProviderLayerId {
                            provider_id: DEMO_PROVIDER_ID,
                            layer_id: LayerId(
                                "collections!GFS_isobaric!temperature!0.01".to_string()
                            )
                        },
                        name: "0.01".to_string(),
                        description: String::new(),
                        properties: vec![],
                    }),
                    CollectionItem::Layer(LayerListing {
                        id: ProviderLayerId {
                            provider_id: DEMO_PROVIDER_ID,
                            layer_id: LayerId(
                                "collections!GFS_isobaric!temperature!1000".to_string()
                            )
                        },
                        name: "1000".to_string(),
                        description: String::new(),
                        properties: vec![],
                    })
                ],
                entry_label: None,
                properties: vec![]
            }
        );

        Ok(())
    }

    #[ge_context::test]
    async fn vector_with_parameter_collection_invalid(
        app_ctx: PostgresContext<NoTls>,
    ) -> Result<()> {
        let collection_id = LayerCollectionId("collections!PointsInGermany!ID".to_string());
        let res = load_layer_collection(
            &collection_id,
            app_ctx.default_session_context().await.unwrap().db(),
        )
        .await;

        assert!(res.is_err());

        Ok(())
    }

    #[ge_context::test]
    async fn raster_with_parameter_without_height_collection_invalid(
        app_ctx: PostgresContext<NoTls>,
    ) -> Result<()> {
        let collection_id =
            LayerCollectionId("collections!GFS_single-level!temperature_max-wind".to_string());
        let res = load_layer_collection(
            &collection_id,
            app_ctx.default_session_context().await.unwrap().db(),
        )
        .await;

        assert!(res.is_err());

        Ok(())
    }

    #[ge_context::test]
    async fn collection_with_parameter_and_height_invalid(
        app_ctx: PostgresContext<NoTls>,
    ) -> Result<()> {
        let collection_id =
            LayerCollectionId("collections!GFS_isobaric!temperature!1000".to_string());
        let res = load_layer_collection(
            &collection_id,
            app_ctx.default_session_context().await.unwrap().db(),
        )
        .await;

        assert!(res.is_err());

        Ok(())
    }

    async fn load_metadata<L, R, Q, D: GeoEngineDb>(
        server: &mut Server,
        collection: &'static str,
        db: D,
    ) -> Box<dyn MetaData<L, R, Q>>
    where
        R: ResultDescriptor,
        dyn DataProvider: MetaDataProvider<L, R, Q>,
    {
        let collection_name = collection.split('!').next().unwrap();
        setup_url(
            server,
            &format!("/collections/{collection_name}"),
            "application/json",
            &format!("edr_{collection_name}.json"),
            1..2,
        )
        .await;

        let provider = create_provider(server, db).await;

        let meta: Box<dyn MetaData<L, R, Q>> = provider
            .meta_data(&DataId::External(ExternalDataId {
                provider_id: DEMO_PROVIDER_ID,
                layer_id: LayerId(format!("collections!{collection}")),
            }))
            .await
            .unwrap();
        server.verify_and_clear();
        meta
    }

    #[ge_context::test]
    async fn generate_ogr_metadata(app_ctx: PostgresContext<NoTls>) {
        let mut server = Server::run();
        let meta = load_metadata::<
            OgrSourceDataset,
            VectorResultDescriptor,
            VectorQueryRectangle,
            PostgresDb<NoTls>,
        >(
            &mut server,
            "PointsInGermany",
            app_ctx.default_session_context().await.unwrap().db(),
        )
        .await;
        let loading_info = meta
            .loading_info(VectorQueryRectangle {
                spatial_bounds: BoundingBox2D::new_unchecked(
                    (-180., -90.).into(),
                    (180., 90.).into(),
                ),
                time_interval: TimeInterval::default(),
                spatial_resolution: SpatialResolution::zero_point_one(),
                attributes: ColumnSelection::all(),
            })
            .await
            .unwrap();
        pretty_assertions::assert_eq!(
            loading_info,
            OgrSourceDataset {
                file_name: format!("/vsicurl_streaming/{}", server.url_str("/collections/PointsInGermany/cube?f=GeoJSON&bbox=-180,-90,180,90&datetime=-262143-01-01T00:00:00%2B00:00%2F%2B262142-12-31T23:59:59.999%2B00:00")).into(),
                layer_name: "cube?f=GeoJSON&bbox=-180,-90,180,90&datetime=-262143-01-01T00:00:00%2B00:00%2F%2B262142-12-31T23:59:59".to_string(),
                data_type: Some(VectorDataType::MultiPoint),
                time: OgrSourceDatasetTimeType::Start {
                    start_field: "time".to_string(),
                    start_format: OgrSourceTimeFormat::Auto,
                    duration: OgrSourceDurationSpec::Zero,
                },
                default_geometry: None,
                columns: Some(OgrSourceColumnSpec {
                    format_specifics: None,
                    x: "geometry".to_string(),
                    y: None,
                    int: vec!["ID".to_string()],
                    float: vec![],
                    text: vec![],
                    bool: vec![],
                    datetime: vec![],
                    rename: None,
                }),
                force_ogr_time_filter: false,
                force_ogr_spatial_filter: false,
                on_error: OgrSourceErrorSpec::Abort,
                sql_query: None,
                attribute_query: None,
                cache_ttl: Default::default(),
            }
        );

        let result_descriptor = meta.result_descriptor().await.unwrap();
        pretty_assertions::assert_eq!(
            result_descriptor,
            VectorResultDescriptor {
                spatial_reference: SpatialReference::epsg_4326().into(),
                data_type: VectorDataType::MultiPoint,
                columns: hashmap! {
                    "ID".to_string() => VectorColumnInfo {
                        data_type: FeatureDataType::Int,
                        measurement: Measurement::Continuous(ContinuousMeasurement {
                            measurement: "ID".to_string(),
                            unit: None,
                        }),
                    }
                },
                time: Some(TimeInterval::new_unchecked(
                    1_672_576_949_000,
                    1_675_255_349_000,
                )),
                bbox: Some(BoundingBox2D::new_unchecked(
                    (-180., -90.).into(),
                    (180., 90.).into()
                )),
            }
        );
    }

    #[ge_context::test]
    #[allow(clippy::too_many_lines)]
    async fn generate_gdal_metadata(app_ctx: PostgresContext<NoTls>) {
        let mut server = Server::run();

        prepare_cube_query(
            &mut server,
            "GFS_isobaric",
            "image/tiff",
            "edr_raster.tif",
            "temperature",
        )
        .await;
        let meta = load_metadata::<
            GdalLoadingInfo,
            RasterResultDescriptor,
            RasterQueryRectangle,
            PostgresDb<NoTls>,
        >(
            &mut server,
            "GFS_isobaric!temperature!1000",
            app_ctx.default_session_context().await.unwrap().db(),
        )
        .await;

        let loading_info_parts = meta
            .loading_info(RasterQueryRectangle {
                spatial_bounds: SpatialPartition2D::new_unchecked(
                    (0., 90.).into(),
                    (360., -90.).into(),
                ),
                time_interval: TimeInterval::new_unchecked(1_692_144_000_000, 1_692_500_400_000),
                spatial_resolution: SpatialResolution::new_unchecked(1., 1.),
                attributes: BandSelection::first(),
            })
            .await
            .unwrap()
            .info
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        pretty_assertions::assert_eq!(
            loading_info_parts,
            vec![
                GdalLoadingInfoTemporalSlice {
                    time: TimeInterval::new_unchecked(
                        1_692_144_000_000, 1_692_154_800_000
                    ),
                    params: Some(GdalDatasetParameters {
                        file_path: format!("/vsicurl_streaming/{}", server.url_str("/collections/GFS_isobaric/cube?bbox=0,-90,359.50000000000006,90&z=1000%2F1000&datetime=2023-08-16T00:00:00Z%2F2023-08-16T00:00:00Z&f=GeoTIFF&parameter-name=temperature")).into(),
                        rasterband_channel: 1,
                        geo_transform: GdalDatasetGeoTransform {
                            origin_coordinate: (0., -90.).into(),
                            x_pixel_size: 0.499_305_555_555_555_6,
                            y_pixel_size: 0.498_614_958_448_753_5,
                        },
                        width: 720,
                        height: 361,
                        file_not_found_handling: FileNotFoundHandling::NoData,
                        no_data_value: None,
                        properties_mapping: None,
                        gdal_open_options: None,
                        gdal_config_options: Some(vec![(
                            "GTIFF_HONOUR_NEGATIVE_SCALEY".to_string(),
                            "YES".to_string(),
                        )]),
                        allow_alphaband_as_mask: false,
                        retry: None,
                    }),
                    cache_ttl: Default::default(),
                },
                GdalLoadingInfoTemporalSlice {
                    time: TimeInterval::new_unchecked(
                        1_692_154_800_000, 1_692_500_400_000
                    ),
                    params: Some(GdalDatasetParameters {
                        file_path: format!("/vsicurl_streaming/{}", server.url_str("/collections/GFS_isobaric/cube?bbox=0,-90,359.50000000000006,90&z=1000%2F1000&datetime=2023-08-16T03:00:00Z%2F2023-08-16T03:00:00Z&f=GeoTIFF&parameter-name=temperature")).into(),
                        rasterband_channel: 1,
                        geo_transform: GdalDatasetGeoTransform {
                            origin_coordinate: (0., -90.).into(),
                            x_pixel_size: 0.499_305_555_555_555_6,
                            y_pixel_size: 0.498_614_958_448_753_5,
                        },
                        width: 720,
                        height: 361,
                        file_not_found_handling: FileNotFoundHandling::NoData,
                        no_data_value: None,
                        properties_mapping: None,
                        gdal_open_options: None,
                        gdal_config_options: Some(vec![(
                            "GTIFF_HONOUR_NEGATIVE_SCALEY".to_string(),
                            "YES".to_string(),
                        )]),
                        allow_alphaband_as_mask: false,
                        retry: None,
                    }),
                    cache_ttl: Default::default(),
                }
            ]
        );

        let result_descriptor = meta.result_descriptor().await.unwrap();
        pretty_assertions::assert_eq!(
            result_descriptor,
            RasterResultDescriptor {
                data_type: RasterDataType::U8,
                spatial_reference: SpatialReference::epsg_4326().into(),
                time: Some(TimeInterval::new_unchecked(
                    1_692_144_000_000,
                    1_692_500_400_000
                )),
                bbox: Some(SpatialPartition2D::new_unchecked(
                    (0., 90.).into(),
                    (359.500_000_000_000_06, -90.).into()
                )),
                resolution: None,
                bands: RasterBandDescriptors::new_single_band(),
            }
        );
    }

    #[ge_context::test]
    async fn query_data(app_ctx: PostgresContext<NoTls>) -> Result<()> {
        let layer_id = "collections!GFS_isobaric!temperature!1000";

        let mut server = Server::run();

        prepare_cube_query(
            &mut server,
            "GFS_isobaric",
            "image/tiff",
            "edr_raster.tif",
            "temperature",
        )
        .await;

        let meta = load_metadata::<
            GdalLoadingInfo,
            RasterResultDescriptor,
            RasterQueryRectangle,
            PostgresDb<NoTls>,
        >(
            &mut server,
            layer_id.strip_prefix("collections!").unwrap(),
            app_ctx.default_session_context().await.unwrap().db(),
        )
        .await;

        let mut exe: MockExecutionContext = MockExecutionContext::test_default();
        exe.add_meta_data(
            DataId::External(ExternalDataId {
                provider_id: DEMO_PROVIDER_ID,
                layer_id: LayerId(layer_id.to_owned()),
            })
            .into(),
            geoengine_datatypes::dataset::NamedData::with_system_provider(
                DEMO_PROVIDER_ID.to_string(),
                layer_id.to_string(),
            ),
            meta,
        );

        let op = GdalSource {
            params: GdalSourceParameters {
                data: geoengine_datatypes::dataset::NamedData::with_system_provider(
                    DEMO_PROVIDER_ID.to_string(),
                    layer_id.to_owned(),
                ),
            },
        }
        .boxed()
        .initialize(WorkflowOperatorPath::initialize_root(), &exe)
        .await
        .unwrap();

        let processor = op.query_processor()?.get_u8().unwrap();

        let spatial_bounds = SpatialPartition2D::new((0., 90.).into(), (180., 0.).into()).unwrap();

        let spatial_resolution = SpatialResolution::new_unchecked(1., 1.);
        let query = RasterQueryRectangle {
            spatial_bounds,
            time_interval: TimeInterval::new_unchecked(1_687_867_200_000, 1_688_806_800_000),
            spatial_resolution,
            attributes: BandSelection::first(),
        };

        let ctx = MockQueryContext::new(ChunkByteSize::MAX);

        let tile_stream = processor.query(query, &ctx).await?;
        assert_eq!(tile_stream.count().await, 2);

        Ok(())
    }
}
