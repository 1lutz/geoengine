use geoengine_datatypes::raster::RasterPropertiesKey;

mod radiance;
mod reflectance;
mod satellite;
mod temperature;

fn slope_key() -> RasterPropertiesKey {
    RasterPropertiesKey {
        domain: Some("msg".into()),
        key: "calibration_slope".into(),
    }
}

fn offset_key() -> RasterPropertiesKey {
    RasterPropertiesKey {
        domain: Some("msg".into()),
        key: "calibration_offset".into(),
    }
}

fn channel_key() -> RasterPropertiesKey {
    RasterPropertiesKey {
        domain: Some("msg".into()),
        key: "channel_number".into(),
    }
}

fn satellite_key() -> RasterPropertiesKey {
    RasterPropertiesKey {
        domain: Some("msg".into()),
        key: "satellite_number".into(),
    }
}

#[cfg(test)]
mod test_util {
    use chrono::{TimeZone, Utc};
    use futures::StreamExt;
    use num_traits::AsPrimitive;

    use geoengine_datatypes::dataset::{DatasetId, InternalDatasetId};
    use geoengine_datatypes::primitives::{
        Measurement, SpatialPartition2D, SpatialResolution, TimeGranularity, TimeInstance,
        TimeInterval, TimeStep,
    };
    use geoengine_datatypes::raster::{
        EmptyGrid2D, GeoTransform, Grid2D, GridOrEmpty, RasterDataType, RasterProperties,
        RasterPropertiesEntry, RasterPropertiesEntryType, RasterTile2D, TileInformation,
    };
    use geoengine_datatypes::spatial_reference::{SpatialReference, SpatialReferenceAuthority};
    use geoengine_datatypes::util::Identifier;

    use crate::engine::{
        MockExecutionContext, MockQueryContext, QueryProcessor, RasterOperator,
        RasterQueryRectangle, RasterResultDescriptor,
    };
    use crate::mock::{MockRasterSource, MockRasterSourceParams};
    use crate::processing::meteosat::{channel_key, offset_key, satellite_key, slope_key};
    use crate::source::{
        FileNotFoundHandling, GdalDatasetParameters, GdalMetaDataRegular, GdalMetadataMapping,
        GdalSource, GdalSourceParameters,
    };
    use crate::util::gdal::raster_dir;
    use crate::util::Result;

    pub(crate) async fn process<T>(
        make_op: T,
        query: RasterQueryRectangle,
        ctx: &MockExecutionContext,
    ) -> Result<RasterTile2D<f32>>
    where
        T: FnOnce() -> Box<dyn RasterOperator>,
    {
        // let input = make_raster(props, custom_data, measurement);

        let op = make_op().initialize(ctx).await?;

        let processor = op.query_processor().unwrap().get_f32().unwrap();

        let ctx = MockQueryContext::default();
        let result_stream = processor.query(query, &ctx).await.unwrap();
        let mut result: Vec<Result<RasterTile2D<f32>>> = result_stream.collect().await;
        assert_eq!(1, result.len());
        result.pop().unwrap()
    }

    pub(crate) fn create_properties(
        channel: Option<u8>,
        satellite: Option<u8>,
        offset: Option<f64>,
        slope: Option<f64>,
    ) -> RasterProperties {
        let mut props = RasterProperties::default();

        if let Some(v) = channel {
            props
                .properties_map
                .insert(channel_key(), RasterPropertiesEntry::Number(v.as_()));
        }

        if let Some(v) = satellite {
            props
                .properties_map
                .insert(satellite_key(), RasterPropertiesEntry::Number(v.as_()));
        }

        if let Some(v) = slope {
            props
                .properties_map
                .insert(slope_key(), RasterPropertiesEntry::Number(v));
        }

        if let Some(v) = offset {
            props
                .properties_map
                .insert(offset_key(), RasterPropertiesEntry::Number(v));
        }
        props
    }

    pub(crate) fn _create_gdal_query() -> RasterQueryRectangle {
        let sr = SpatialResolution::new_unchecked(3_000.403_165_817_261, 3_000.403_165_817_261);
        let ul = (0., 0.).into();
        let lr = (599. * sr.x, -599. * sr.y).into();
        RasterQueryRectangle {
            spatial_bounds: SpatialPartition2D::new_unchecked(ul, lr),
            time_interval: TimeInterval::new_unchecked(
                TimeInstance::from(
                    Utc.datetime_from_str("20121212_1200", "%Y%m%d_%H%M")
                        .unwrap(),
                ),
                TimeInstance::from(
                    Utc.datetime_from_str("20121212_1215", "%Y%m%d_%H%M")
                        .unwrap(),
                ),
            ),
            spatial_resolution: sr,
        }
    }

    pub(crate) fn create_mock_query() -> RasterQueryRectangle {
        RasterQueryRectangle {
            spatial_bounds: SpatialPartition2D::new_unchecked((0., 4.).into(), (3., 0.).into()),
            time_interval: Default::default(),
            spatial_resolution: SpatialResolution::one(),
        }
    }

    pub(crate) fn create_mock_source(
        props: RasterProperties,
        custom_data: Option<Vec<u8>>,
        measurement: Option<Measurement>,
    ) -> MockRasterSource {
        let no_data_value = Some(0);

        let raster = match custom_data {
            Some(v) if v.is_empty() => {
                GridOrEmpty::Empty(EmptyGrid2D::new([3, 2].into(), no_data_value.unwrap()))
            }
            Some(v) => GridOrEmpty::Grid(Grid2D::new([3, 2].into(), v, no_data_value).unwrap()),
            None => GridOrEmpty::Grid(
                Grid2D::new(
                    [3, 2].into(),
                    vec![1, 2, 3, 4, 5, no_data_value.unwrap()],
                    no_data_value,
                )
                .unwrap(),
            ),
        };

        let raster_tile = RasterTile2D::new_with_tile_info_and_properties(
            TimeInterval::default(),
            TileInformation {
                global_tile_position: [-1, 0].into(),
                tile_size_in_pixels: [3, 2].into(),
                global_geo_transform: Default::default(),
            },
            raster,
            props,
        );

        MockRasterSource {
            params: MockRasterSourceParams {
                data: vec![raster_tile],
                result_descriptor: RasterResultDescriptor {
                    data_type: RasterDataType::F32,
                    spatial_reference: SpatialReference::epsg_4326().into(),
                    measurement: measurement.unwrap_or_else(|| Measurement::Continuous {
                        measurement: "raw".to_string(),
                        unit: None,
                    }),
                    no_data_value: no_data_value.map(AsPrimitive::as_),
                },
            },
        }
    }

    pub(crate) fn _create_gdal_src(ctx: &mut MockExecutionContext) -> GdalSource {
        let dataset_id: DatasetId = InternalDatasetId::new().into();

        let timestamp = Utc
            .datetime_from_str("20121212_1200", "%Y%m%d_%H%M")
            .unwrap();

        let no_data_value = Some(0.);
        let meta = GdalMetaDataRegular {
            start: TimeInstance::from(timestamp),
            step: TimeStep {
                granularity: TimeGranularity::Minutes,
                step: 15,
            },
            placeholder: "%%%_START_TIME_%%%".to_string(),
            time_format: "%Y%m%d_%H%M".to_string(),
            params: GdalDatasetParameters {
                file_path: raster_dir().join("msg/%%%_START_TIME_%%%.tif"),
                rasterband_channel: 1,
                geo_transform: GeoTransform {
                    origin_coordinate: (-5_570_248.477_339_745, 5_570_248.477_339_745).into(),
                    x_pixel_size: 3_000.403_165_817_261,
                    y_pixel_size: -3_000.403_165_817_261,
                },
                width: 3712,
                height: 3712,
                file_not_found_handling: FileNotFoundHandling::Error,
                no_data_value,
                properties_mapping: Some(vec![
                    GdalMetadataMapping::identity(
                        satellite_key(),
                        RasterPropertiesEntryType::Number,
                    ),
                    GdalMetadataMapping::identity(channel_key(), RasterPropertiesEntryType::Number),
                    GdalMetadataMapping::identity(offset_key(), RasterPropertiesEntryType::Number),
                    GdalMetadataMapping::identity(slope_key(), RasterPropertiesEntryType::Number),
                ]),
                gdal_open_options: None,
            },
            result_descriptor: RasterResultDescriptor {
                data_type: RasterDataType::I16,
                spatial_reference: SpatialReference::new(SpatialReferenceAuthority::SrOrg, 81)
                    .into(),
                measurement: Measurement::Continuous {
                    measurement: "raw".to_string(),
                    unit: None,
                },
                no_data_value,
            },
        };
        ctx.add_meta_data(dataset_id.clone(), Box::new(meta));

        GdalSource {
            params: GdalSourceParameters {
                dataset: dataset_id,
            },
        }
    }
}