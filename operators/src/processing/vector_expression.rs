use crate::engine::{
    CanonicOperatorName, ExecutionContext, InitializedSources, InitializedVectorOperator, Operator,
    OperatorName, QueryContext, SingleVectorSource, TypedVectorQueryProcessor, VectorColumnInfo,
    VectorOperator, VectorQueryProcessor, VectorResultDescriptor, WorkflowOperatorPath,
};
use crate::util::Result;
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use geoengine_datatypes::collections::{
    FeatureCollection, FeatureCollectionInfos, FeatureCollectionModifications,
    IntoGeometryOptionsIterator, VectorDataType,
};
use geoengine_datatypes::primitives::{
    AsGeoOption, FeatureData, FeatureDataRef, FeatureDataType, FloatOptionsParIter, Geometry,
    Measurement, MultiLineStringRef, MultiPointRef, MultiPolygonRef, NoGeometry,
    VectorQueryRectangle,
};
use geoengine_datatypes::util::arrow::ArrowTyped;
use geoengine_expression::{
    DataType, ExpressionDependencies, ExpressionParser, LinkedExpression,
    Parameter as ExpressionParameter,
};
use rayon::iter::{IndexedParallelIterator, IntoParallelIterator, ParallelIterator};
use serde::{Deserialize, Serialize};
use snafu::{ResultExt, Snafu};
use std::collections::HashMap;
use std::sync::Arc;

/// A vector expression creates or replaces a column in a `FeatureCollection` by evaluating an expression.
/// The expression receives the feature's columns as variables.
pub type VectorExpression = Operator<VectorExpressionParams, SingleVectorSource>;

impl OperatorName for VectorExpression {
    const TYPE_NAME: &'static str = "VectorExpression";
}

const MAX_INPUT_COLUMNS: usize = 2;
const PARALLEL_MIN_BATCH_SIZE: usize = 32; // TODO: find good default

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VectorExpressionParams {
    /// The columns to use as variables in the expression.
    // TODO: Columns with special characaters? Use `ticks`?
    pub input_columns: Vec<String>,

    /// The expression to evaluate.
    pub expression: String,

    /// The type and name of the new column.
    pub output_column: OutputColumn,

    /// The expression will always include the geometry column.
    /// Thus, it is necessary to specify the variable name of the geometry column.
    /// The default is `geom`.
    #[serde(default = "geometry_default_column_name")]
    pub geometry_column_name: String,

    /// The measurement of the new column.
    /// The default is [`Measurement::Unitless`].
    #[serde(default)]
    pub output_measurement: Measurement,
}

fn geometry_default_column_name() -> String {
    "geom".into()
}

/// Specify the output of the expression.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "camelCase")]
pub enum OutputColumn {
    /// The expression will override the current geometry
    Geometry(VectorDataType),
    /// The expression will append a new `Float` column
    // TODO: allow more types than `Float`s
    Column(String),
}

struct InitializedVectorExpression {
    name: CanonicOperatorName,
    result_descriptor: VectorResultDescriptor,
    features: Box<dyn InitializedVectorOperator>,
    expression: Arc<LinkedExpression>,
    input_columns: Vec<String>,
    geometry_column_name: String,
    output_column: OutputColumn,
}

#[typetag::serde]
#[async_trait]
impl VectorOperator for VectorExpression {
    async fn _initialize(
        self: Box<Self>,
        path: WorkflowOperatorPath,
        context: &dyn ExecutionContext,
    ) -> Result<Box<dyn InitializedVectorOperator>> {
        // TODO: This is super ugly to being forced to do this on every operator. This must be refactored.
        let name = CanonicOperatorName::from(&self);

        if self.params.input_columns.len() > MAX_INPUT_COLUMNS {
            Err(VectorExpressionError::TooManyInputColumns {
                max: MAX_INPUT_COLUMNS,
                found: self.params.input_columns.len(),
            })?;
        }

        let initialized_source = self.sources.initialize_sources(path, context).await?;

        // we can reuse the result descriptor, because we only add a column later on
        let mut result_descriptor = initialized_source.vector.result_descriptor().clone();

        check_input_column_validity(&result_descriptor.columns, &self.params.input_columns)?;
        check_output_column_validity(&self.params.output_column)?;

        let expression_output_type = match &self.params.output_column {
            OutputColumn::Geometry(vector_data_type) => {
                result_descriptor.data_type = *vector_data_type;
                match vector_data_type {
                    VectorDataType::MultiPoint => DataType::MultiPoint,
                    VectorDataType::MultiLineString => DataType::MultiLineString,
                    VectorDataType::MultiPolygon => DataType::MultiPolygon,
                    VectorDataType::Data => Err(VectorExpressionError::CannotGenerateDataOutput)?,
                }
            }
            OutputColumn::Column(output_column_name) => {
                insert_new_column(
                    &mut result_descriptor.columns,
                    output_column_name.clone(),
                    self.params.output_measurement,
                )?;
                DataType::Number
            }
        };

        let expression = Arc::new(compile_expression(
            &self.params.expression,
            self.params.geometry_column_name.clone(),
            result_descriptor.data_type,
            &self.params.input_columns,
            expression_output_type,
        )?);

        let initialized_operator = InitializedVectorExpression {
            name,
            result_descriptor,
            features: initialized_source.vector,
            expression,
            input_columns: self.params.input_columns,
            geometry_column_name: self.params.geometry_column_name,
            output_column: self.params.output_column,
        };

        Ok(initialized_operator.boxed())
    }

    span_fn!(VectorExpression);
}

fn check_input_column_validity(
    columns: &HashMap<String, VectorColumnInfo>,
    input_columns: &[String],
) -> Result<(), VectorExpressionError> {
    for input_column in input_columns {
        if input_column.contains(|c: char| !c.is_alphanumeric()) {
            Err(VectorExpressionError::ColumnNameContainsSpecialCharacters {
                name: input_column.clone(),
            })?;
        }

        let Some(column_info) = columns.get(input_column) else {
            return Err(VectorExpressionError::InputColumnNotExisting {
                name: input_column.clone(),
            });
        };

        match column_info.data_type {
            FeatureDataType::Float | FeatureDataType::Int => {}
            _ => Err(VectorExpressionError::InputColumnNotNumeric {
                name: input_column.clone(),
            })?,
        }
    }

    Ok(())
}

fn check_output_column_validity(output_column: &OutputColumn) -> Result<(), VectorExpressionError> {
    match output_column {
        OutputColumn::Geometry(_) => {}
        OutputColumn::Column(column) => {
            if column.contains(|c: char| !c.is_alphanumeric()) {
                Err(VectorExpressionError::ColumnNameContainsSpecialCharacters {
                    name: column.clone(),
                })?;
            }
        }
    };

    Ok(())
}

fn insert_new_column(
    columns: &mut HashMap<String, VectorColumnInfo>,
    name: String,
    measurement: Measurement,
) -> Result<(), VectorExpressionError> {
    let output_column_collision = columns.insert(
        name.clone(),
        VectorColumnInfo {
            data_type: FeatureDataType::Float,
            measurement,
        },
    );

    if output_column_collision.is_some() {
        return Err(VectorExpressionError::OutputColumnCollision { name });
    }

    Ok(())
}

fn compile_expression(
    expression_code: &str,
    geom_name: String,
    geom_type: VectorDataType,
    parameters: &[String],
    output_type: DataType,
) -> Result<LinkedExpression, VectorExpressionError> {
    let geom_parameter = match geom_type {
        VectorDataType::Data | VectorDataType::MultiPoint => {
            ExpressionParameter::MultiPoint(geom_name.into())
        }
        VectorDataType::MultiLineString => ExpressionParameter::MultiLineString(geom_name.into()),
        VectorDataType::MultiPolygon => ExpressionParameter::MultiPolygon(geom_name.into()),
    };
    let mut expression_parameters = Vec::with_capacity(parameters.len() + 1);
    expression_parameters.push(geom_parameter);
    expression_parameters.extend(
        parameters
            .iter()
            .map(|p| ExpressionParameter::Number(p.into())),
    );
    let expression = ExpressionParser::new(&expression_parameters, output_type)?.parse(
        "expression", // TODO: generate and store a unique name
        expression_code,
    )?;

    // TODO: (a) use a cache for the expression dependencies
    // TODO: (b) spawn a blocking task for the compilation process
    let expression_dependencies = ExpressionDependencies::new()?;

    eprintln!("{}", expression.pretty_code()); // TODO: remove

    Ok(LinkedExpression::from_ast(
        &expression,
        &expression_dependencies,
    )?)
}

impl InitializedVectorOperator for InitializedVectorExpression {
    fn result_descriptor(&self) -> &VectorResultDescriptor {
        &self.result_descriptor
    }

    fn query_processor(&self) -> Result<TypedVectorQueryProcessor> {
        let source_processor = self.features.query_processor()?;

        Ok(match (source_processor, self.output_column.clone()) {
            (TypedVectorQueryProcessor::Data(source), OutputColumn::Column(output_column)) => {
                VectorExpressionColumnProcessor {
                    source,
                    result_descriptor: self.result_descriptor.clone(),
                    expression: self.expression.clone(),
                    input_columns: self.input_columns.clone(),
                    // geometry_column_name: self.geometry_column_name.clone(),
                    output_column,
                }
                .boxed()
                .into()
            }
            (
                TypedVectorQueryProcessor::MultiPoint(source),
                OutputColumn::Column(output_column),
            ) => VectorExpressionColumnProcessor {
                source,
                result_descriptor: self.result_descriptor.clone(),
                expression: self.expression.clone(),
                input_columns: self.input_columns.clone(),
                // geometry_column_name: self.geometry_column_name.clone(),
                output_column,
            }
            .boxed()
            .into(),
            (
                TypedVectorQueryProcessor::MultiLineString(source),
                OutputColumn::Column(output_column),
            ) => VectorExpressionColumnProcessor {
                source,
                result_descriptor: self.result_descriptor.clone(),
                expression: self.expression.clone(),
                input_columns: self.input_columns.clone(),
                // geometry_column_name: self.geometry_column_name.clone(),
                output_column,
            }
            .boxed()
            .into(),
            (
                TypedVectorQueryProcessor::MultiPolygon(source),
                OutputColumn::Column(output_column),
            ) => VectorExpressionColumnProcessor {
                source,
                result_descriptor: self.result_descriptor.clone(),
                expression: self.expression.clone(),
                input_columns: self.input_columns.clone(),
                // geometry_column_name: self.geometry_column_name.clone(),
                output_column,
            }
            .boxed()
            .into(),
            _ => todo!(),
        })
    }

    fn canonic_name(&self) -> CanonicOperatorName {
        self.name.clone()
    }
}

pub struct VectorExpressionColumnProcessor<Q, G>
where
    G: Geometry,
    Q: VectorQueryProcessor<VectorType = FeatureCollection<G>>,
{
    source: Q,
    result_descriptor: VectorResultDescriptor,
    expression: Arc<LinkedExpression>,
    input_columns: Vec<String>,
    // geometry_column_name: String,
    output_column: String,
}

// pub struct VectorExpressionGeometryProcessor<Q, GIn, GOut>
// where
//     GIn: Geometry + 'static + Sized,
//     GOut: Geometry + 'static + Sized,
//     Q: VectorQueryProcessor<VectorType = FeatureCollection<GIn>> + 'static + Sized,
//     // TODO: FeatureCollection<G>: GeoFeatureCollectionModifications<G>,
// {
//     source: Q,
//     result_descriptor: VectorResultDescriptor,
//     expression: Arc<LinkedExpression>,
//     input_columns: Vec<String>,
//     _out: PhantomData<GOut>,
// }

type ExpressionGeometryType<'g, G> = <<FeatureCollection<G> as IntoGeometryOptionsIterator<'g>>::GeometryType as AsExpressionGeo>::ExpressionGeometryType;

#[async_trait]
impl<Q, G> VectorQueryProcessor for VectorExpressionColumnProcessor<Q, G>
where
    Q: VectorQueryProcessor<VectorType = FeatureCollection<G>>,
    G: Geometry + ArrowTyped + 'static,
    FeatureCollection<G>: for<'g> IntoGeometryOptionsIterator<'g> + 'static,
    for<'g> <FeatureCollection<G> as IntoGeometryOptionsIterator<'g>>::GeometryType:
        AsExpressionGeo + Send,
    for<'g> <<FeatureCollection<G> as IntoGeometryOptionsIterator<'g>>::GeometryOptionIterator as IntoParallelIterator>::Iter:
        IndexedParallelIterator + Send,
{
    type VectorType = FeatureCollection<G>;


    async fn vector_query<'a>(
        &'a self,
        query: VectorQueryRectangle,
        ctx: &'a dyn QueryContext,
    ) -> Result<BoxStream<'a, Result<Self::VectorType>>> {

        let stream = self.source.vector_query(query, ctx).await?;

        let stream = stream.then(move |collection| async move {
            let collection = collection?;
            let input_columns = self.input_columns.clone();
            let output_column = self.output_column.clone();
            let expression = self.expression.clone();

            crate::util::spawn_blocking_with_thread_pool(ctx.thread_pool().clone(), move || {
                let inputs: Vec<FeatureDataRef> = input_columns
                    .into_iter()
                    .map(|input_column| {
                        collection
                            .data(&input_column)
                            .expect("was checked durin initialization")
                    })
                    .collect();

                let float_inputs: Vec<FloatOptionsParIter> = inputs
                    .iter()
                    .map(FeatureDataRef::float_options_par_iter)
                    .collect::<Vec<_>>();

                // TODO: custom type for geometry input
                let geom_input = collection.geometry_options().into_par_iter().map(|geometry_option| {
                    if let Some(geometry) = geometry_option.as_ref() {
                        geometry.as_expression_geo()
                    } else {
                        None
                    }
                });

                let result: Vec<Option<f64>> = match float_inputs.len() {
                    0 => {
                        let f = unsafe {
                            expression.function_nary::<fn(
                                Option<ExpressionGeometryType<'_, G>>,
                            ) -> Option<f64>>()
                        }
                        .map_err(VectorExpressionError::from)?;

                        Ok(geom_input
                            .with_min_len(PARALLEL_MIN_BATCH_SIZE)
                            .map(*f)
                            .collect::<Vec<_>>())
                    }
                    1 => {
                        let [a] = <[_; 1]>::try_from(float_inputs)
                            .expect("it matches the match condition");
                        let f =
                            unsafe { expression.function_nary::<fn(
                                Option<ExpressionGeometryType<'_, G>>,
                                Option<f64>,
                            ) -> Option<f64>>() }
                                .map_err(VectorExpressionError::from)?;

                            Ok((geom_input, a)
                                .into_par_iter()
                                .with_min_len(PARALLEL_MIN_BATCH_SIZE)
                                .map(|(geom, a)| f(geom, a))
                                .collect::<Vec<_>>())
                    }
                    2 => {
                        let [a, b] = <[_; 2]>::try_from(float_inputs)
                            .expect("it matches the match condition");
                        let f = unsafe {
                            expression
                                .function_nary::<fn(
                                    Option<ExpressionGeometryType<'_, G>>,
                                    Option<f64>,
                                    Option<f64>,
                                ) -> Option<f64>>()
                        }
                        .map_err(VectorExpressionError::from)?;

                        Ok((geom_input, a, b)
                                .into_par_iter()
                                .with_min_len(PARALLEL_MIN_BATCH_SIZE)
                                .map(|(geom, a, b)| f(geom, a, b))
                                .collect::<Vec<_>>())
                    }
                    // TODO: implement more cases
                    other => Err(VectorExpressionError::TooManyInputColumns {
                        max: MAX_INPUT_COLUMNS,
                        found: other,
                    }),
                }?;

                Ok(collection
                    .add_column(&output_column, FeatureData::NullableFloat(result))
                    .context(error::AddColumn {
                        name: output_column,
                    })?)
            })
            .await?
        });

        Ok(stream.boxed())
    }

    fn vector_result_descriptor(&self) -> &VectorResultDescriptor {
        &self.result_descriptor
    }
}

// fn call_expression_function<GIn, GOut>(
//     expression: &Arc<LinkedExpression>,
//     collection: &FeatureCollection<G>,
//     input_columns: &[String],
//     output_column: &str,
// ) -> Result<FeatureCollection<G>, VectorExpressionError>
// where
//     G: Geometry + ArrowTyped + 'static,
//     for<'i> FeatureCollection<G>: IntoGeometryOptionsIterator<'i>,
// {
//     todo!()
// }

/// Convenience trait for converting [`geoengine_datatypes`] types to [`geoengine_expression`] types.
trait AsExpressionGeo: AsGeoOption {
    type ExpressionGeometryType: Send;

    fn as_expression_geo(&self) -> Option<Self::ExpressionGeometryType>;
}

impl<'c> AsExpressionGeo for MultiPointRef<'c> {
    type ExpressionGeometryType = geoengine_expression::MultiPoint;

    fn as_expression_geo(&self) -> Option<Self::ExpressionGeometryType> {
        self.as_geo_option().map(Into::into)
    }
}

impl<'c> AsExpressionGeo for MultiLineStringRef<'c> {
    type ExpressionGeometryType = geoengine_expression::MultiLineString;

    fn as_expression_geo(&self) -> Option<Self::ExpressionGeometryType> {
        self.as_geo_option().map(Into::into)
    }
}

impl<'c> AsExpressionGeo for MultiPolygonRef<'c> {
    type ExpressionGeometryType = geoengine_expression::MultiPolygon;

    fn as_expression_geo(&self) -> Option<Self::ExpressionGeometryType> {
        self.as_geo_option().map(Into::into)
    }
}

impl AsExpressionGeo for NoGeometry {
    type ExpressionGeometryType = geoengine_expression::MultiPoint;

    fn as_expression_geo(&self) -> Option<Self::ExpressionGeometryType> {
        self.as_geo_option().map(Into::into)
    }
}

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)), context(suffix(false)), module(error))]
pub enum VectorExpressionError {
    #[snafu(display("Input column `{name}` does not exist."))]
    InputColumnNotExisting { name: String },

    #[snafu(display("Input column `{name}` is not numeric."))]
    InputColumnNotNumeric { name: String },

    #[snafu(display("Found {found} columns, but only up to {max} are allowed."))]
    TooManyInputColumns { max: usize, found: usize },

    #[snafu(display("The column `{name}` contains special characters."))]
    ColumnNameContainsSpecialCharacters { name: String },

    #[snafu(display("Output column `{name}` already exists."))]
    OutputColumnCollision { name: String },

    #[snafu(display("Cannot create `DataCollection`."))]
    CannotGenerateDataOutput,

    #[snafu(display("Cannot parse expression: {source}"), context(false))]
    Parsing {
        source: geoengine_expression::error::ExpressionParserError,
    },

    #[snafu(display("Cannot call expression function: {source}."), context(false))]
    Executing {
        source: geoengine_expression::error::ExpressionExecutionError,
    },

    #[snafu(display("Cannot add column {name}."))]
    AddColumn {
        name: String,
        source: geoengine_datatypes::error::Error,
    },

    #[snafu(display("Could not replace geometries."))]
    ReplaceGeometries {
        source: geoengine_datatypes::error::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        engine::{ChunkByteSize, MockExecutionContext, MockQueryContext, QueryProcessor},
        mock::MockFeatureCollectionSource,
    };
    use geoengine_datatypes::{
        collections::{ChunksEqualIgnoringCacheHint, MultiPointCollection, MultiPolygonCollection},
        primitives::{
            BoundingBox2D, ColumnSelection, MultiPoint, MultiPolygon, SpatialResolution,
            TimeInterval,
        },
        util::test::TestDefault,
    };

    #[test]
    fn it_deserializes_the_operator() {
        let def: Operator<VectorExpressionParams, SingleVectorSource> = VectorExpression {
            params: VectorExpressionParams {
                input_columns: vec!["foo".into(), "bar".into()],
                expression: "foo + bar".into(),
                output_column: OutputColumn::Column("baz".into()),
                output_measurement: Measurement::Unitless,
                geometry_column_name: "geom".to_string(),
            },
            sources: MockFeatureCollectionSource::<MultiPoint>::multiple(vec![])
                .boxed()
                .into(),
        };

        let json = serde_json::json!({
            "params": {
                "inputColumns": ["foo", "bar"],
                "expression": "foo + bar",
                "outputColumn": {
                    "type": "column",
                    "value": "baz",
                },
                "outputMeasurement": {
                    "type": "unitless",
                },
                "geometryColumnName": "geom",
            },
            "sources": {
                "vector": {
                    "type": "MockFeatureCollectionSourceMultiPoint",
                    "params": {
                        "collections": [],
                        "spatialReference": "EPSG:4326",
                        "measurements": null,
                    }
                }
            }
        });

        assert_eq!(serde_json::to_value(&def).unwrap(), json.clone());
        let _operator: VectorExpression = serde_json::from_value(json).unwrap();
    }

    #[tokio::test]
    async fn it_computes_unary_float_expressions() {
        let points = MultiPointCollection::from_slices(
            MultiPoint::many(vec![(0.0, 0.1), (1.0, 1.1), (2.0, 3.1)])
                .unwrap()
                .as_ref(),
            &[TimeInterval::new_unchecked(0, 1); 3],
            &[(
                "foo",
                FeatureData::NullableFloat(vec![Some(1.0), None, Some(3.0)]),
            )],
        )
        .unwrap();

        let point_source = MockFeatureCollectionSource::single(points.clone()).boxed();

        let operator = VectorExpression {
            params: VectorExpressionParams {
                input_columns: vec!["foo".into()],
                expression: "2 * foo".into(),
                output_column: OutputColumn::Column("bar".into()),
                output_measurement: Measurement::Unitless,
                geometry_column_name: "geom".to_string(),
            },
            sources: point_source.into(),
        }
        .boxed()
        .initialize(
            WorkflowOperatorPath::initialize_root(),
            &MockExecutionContext::test_default(),
        )
        .await
        .unwrap();

        let query_processor = operator.query_processor().unwrap().multi_point().unwrap();

        let query_rectangle = VectorQueryRectangle {
            spatial_bounds: BoundingBox2D::new((0., 0.).into(), (10., 10.).into()).unwrap(),
            time_interval: TimeInterval::default(),
            spatial_resolution: SpatialResolution::zero_point_one(),
            attributes: ColumnSelection::all(),
        };
        let ctx = MockQueryContext::new(ChunkByteSize::MAX);

        let query = query_processor.query(query_rectangle, &ctx).await.unwrap();

        let mut result = query
            .map(Result::unwrap)
            .collect::<Vec<MultiPointCollection>>()
            .await;

        assert_eq!(result.len(), 1);
        let result = result.remove(0);

        let expected_result = MultiPointCollection::from_slices(
            MultiPoint::many(vec![(0.0, 0.1), (1.0, 1.1), (2.0, 3.1)])
                .unwrap()
                .as_ref(),
            &[TimeInterval::new_unchecked(0, 1); 3],
            &[
                (
                    "foo",
                    FeatureData::NullableFloat(vec![Some(1.0), None, Some(3.0)]),
                ),
                (
                    "bar",
                    FeatureData::NullableFloat(vec![Some(2.0), None, Some(6.0)]),
                ),
            ],
        )
        .unwrap();

        // TODO: maybe it is nicer to have something wrapping the actual data that we care about and just adds some cache info
        assert!(
            result.chunks_equal_ignoring_cache_hint(&expected_result),
            "{result:#?} != {expected_result:#?}",
        );
    }

    #[tokio::test]
    async fn it_computes_binary_float_expressions() {
        let points = MultiPointCollection::from_slices(
            MultiPoint::many(vec![
                (0.0, 0.1),
                (1.0, 1.1),
                (2.0, 2.1),
                (3.0, 3.1),
                (4.0, 4.1),
            ])
            .unwrap()
            .as_ref(),
            &[TimeInterval::new_unchecked(0, 1); 5],
            &[
                (
                    "foo",
                    FeatureData::NullableFloat(vec![Some(1.0), None, Some(3.0), None, Some(5.0)]),
                ),
                (
                    "bar",
                    FeatureData::NullableInt(vec![Some(10), None, None, Some(40), Some(50)]),
                ),
            ],
        )
        .unwrap();

        let point_source = MockFeatureCollectionSource::single(points.clone()).boxed();

        let operator = VectorExpression {
            params: VectorExpressionParams {
                input_columns: vec!["foo".into(), "bar".into()],
                expression: "foo + bar".into(),
                output_column: OutputColumn::Column("baz".into()),
                output_measurement: Measurement::Unitless,
                geometry_column_name: "geom".to_string(),
            },
            sources: point_source.into(),
        }
        .boxed()
        .initialize(
            WorkflowOperatorPath::initialize_root(),
            &MockExecutionContext::test_default(),
        )
        .await
        .unwrap();

        let query_processor = operator.query_processor().unwrap().multi_point().unwrap();

        let query_rectangle = VectorQueryRectangle {
            spatial_bounds: BoundingBox2D::new((0., 0.).into(), (10., 10.).into()).unwrap(),
            time_interval: TimeInterval::default(),
            spatial_resolution: SpatialResolution::zero_point_one(),
            attributes: ColumnSelection::all(),
        };
        let ctx = MockQueryContext::new(ChunkByteSize::MAX);

        let query = query_processor.query(query_rectangle, &ctx).await.unwrap();

        let mut result = query
            .map(Result::unwrap)
            .collect::<Vec<MultiPointCollection>>()
            .await;

        assert_eq!(result.len(), 1);
        let result = result.remove(0);

        let expected_result = MultiPointCollection::from_slices(
            MultiPoint::many(vec![
                (0.0, 0.1),
                (1.0, 1.1),
                (2.0, 2.1),
                (3.0, 3.1),
                (4.0, 4.1),
            ])
            .unwrap()
            .as_ref(),
            &[TimeInterval::new_unchecked(0, 1); 5],
            &[
                (
                    "foo",
                    FeatureData::NullableFloat(vec![Some(1.0), None, Some(3.0), None, Some(5.0)]),
                ),
                (
                    "bar",
                    FeatureData::NullableInt(vec![Some(10), None, None, Some(40), Some(50)]),
                ),
                (
                    "baz",
                    FeatureData::NullableFloat(vec![Some(11.0), None, None, None, Some(55.0)]),
                ),
            ],
        )
        .unwrap();

        // TODO: maybe it is nicer to have something wrapping the actual data that we care about and just adds some cache info
        assert!(
            result.chunks_equal_ignoring_cache_hint(&expected_result),
            "{result:#?} != {expected_result:#?}",
        );
    }

    #[tokio::test]
    async fn it_computes_the_area_from_a_geom() {
        let polygons = MockFeatureCollectionSource::single(
            MultiPolygonCollection::from_slices(
                &[
                    MultiPolygon::new(vec![vec![vec![
                        (0.5, -0.5).into(),
                        (4., -1.).into(),
                        (0.5, -2.5).into(),
                        (0.5, -0.5).into(),
                    ]]])
                    .unwrap(),
                    MultiPolygon::new(vec![vec![vec![
                        (0.5, -0.5).into(),
                        (4., -1.).into(),
                        (0.5, -2.5).into(),
                        (0.5, -0.5).into(),
                    ]]])
                    .unwrap(),
                ],
                &[TimeInterval::new_unchecked(0, 1); 2],
                &[("foo", FeatureData::NullableFloat(vec![Some(1.0), None]))],
            )
            .unwrap(),
        )
        .boxed();

        let result = compute_result::<MultiPolygonCollection>(
            VectorExpression {
                params: VectorExpressionParams {
                    input_columns: vec![],
                    expression: "2".into(),
                    output_column: OutputColumn::Column("area".into()),
                    output_measurement: Measurement::Unitless,
                    geometry_column_name: "geom".to_string(),
                },
                sources: polygons.into(),
            },
            VectorQueryRectangle {
                spatial_bounds: BoundingBox2D::new((0., 0.).into(), (10., 10.).into()).unwrap(),
                time_interval: TimeInterval::default(),
                spatial_resolution: SpatialResolution::zero_point_one(),
                attributes: ColumnSelection::all(),
            },
        )
        .await;

        let expected_result = MultiPolygonCollection::from_slices(
            &[
                MultiPolygon::new(vec![vec![vec![
                    (0.5, -0.5).into(),
                    (4., -1.).into(),
                    (0.5, -2.5).into(),
                    (0.5, -0.5).into(),
                ]]])
                .unwrap(),
                MultiPolygon::new(vec![vec![vec![
                    (0.5, -0.5).into(),
                    (4., -1.).into(),
                    (0.5, -2.5).into(),
                    (0.5, -0.5).into(),
                ]]])
                .unwrap(),
            ],
            &[TimeInterval::new_unchecked(0, 1); 2],
            &[
                ("foo", FeatureData::NullableFloat(vec![Some(1.0), None])),
                (
                    "area",
                    FeatureData::NullableFloat(vec![Some(2.0), Some(2.0)]),
                ),
            ],
        )
        .unwrap();

        // TODO: maybe it is nicer to have something wrapping the actual data that we care about and just adds some cache info
        assert!(
            result.chunks_equal_ignoring_cache_hint(&expected_result),
            "{result:#?} != {expected_result:#?}",
        );
    }

    #[tokio::test]
    async fn it_computes_the_centroid_of_a_geom() {
        let polygons = MockFeatureCollectionSource::single(
            MultiPolygonCollection::from_slices(
                &[
                    MultiPolygon::new(vec![vec![vec![
                        (0.5, -0.5).into(),
                        (4., -1.).into(),
                        (0.5, -2.5).into(),
                        (0.5, -0.5).into(),
                    ]]])
                    .unwrap(),
                    MultiPolygon::new(vec![vec![vec![
                        (0.5, -0.5).into(),
                        (4., -1.).into(),
                        (0.5, -2.5).into(),
                        (0.5, -0.5).into(),
                    ]]])
                    .unwrap(),
                ],
                &[TimeInterval::new_unchecked(0, 1); 2],
                &[("foo", FeatureData::NullableFloat(vec![Some(1.0), None]))],
            )
            .unwrap(),
        )
        .boxed();

        let result = compute_result::<MultiPointCollection>(
            VectorExpression {
                params: VectorExpressionParams {
                    input_columns: vec![],
                    expression: "centroid(geom)".into(),
                    output_column: OutputColumn::Geometry(VectorDataType::MultiPoint),
                    output_measurement: Measurement::Unitless,
                    geometry_column_name: "geom".to_string(),
                },
                sources: polygons.into(),
            },
            VectorQueryRectangle {
                spatial_bounds: BoundingBox2D::new((0., 0.).into(), (10., 10.).into()).unwrap(),
                time_interval: TimeInterval::default(),
                spatial_resolution: SpatialResolution::zero_point_one(),
                attributes: ColumnSelection::all(),
            },
        )
        .await;

        let expected_result = MultiPointCollection::from_slices(
            MultiPoint::many(vec![(0.0, 0.1), (1.0, 1.1)])
                .unwrap()
                .as_ref(),
            &[TimeInterval::new_unchecked(0, 1); 2],
            &[("foo", FeatureData::NullableFloat(vec![Some(1.0), None]))],
        )
        .unwrap();

        // TODO: maybe it is nicer to have something wrapping the actual data that we care about and just adds some cache info
        assert!(
            result.chunks_equal_ignoring_cache_hint(&expected_result),
            "{result:#?} != {expected_result:#?}",
        );
    }

    async fn compute_result<C>(operator: VectorExpression, query_rectangle: VectorQueryRectangle) -> C
    where
        C: 'static,
        Box<dyn VectorQueryProcessor<VectorType = C>>: TryFrom<TypedVectorQueryProcessor>,
        <Box<dyn VectorQueryProcessor<VectorType = C>> as TryFrom<TypedVectorQueryProcessor>>::Error: std::fmt::Debug,
    {
        let operator = operator
            .boxed()
            .initialize(
                WorkflowOperatorPath::initialize_root(),
                &MockExecutionContext::test_default(),
            )
            .await
            .unwrap();

        let query_processor: Box<dyn VectorQueryProcessor<VectorType = C>> =
            operator.query_processor().unwrap().try_into().unwrap();

        let ctx = MockQueryContext::new(ChunkByteSize::MAX);

        let query = query_processor.query(query_rectangle, &ctx).await.unwrap();

        let mut result = query.map(Result::unwrap).collect::<Vec<C>>().await;

        assert_eq!(result.len(), 1);
        result.remove(0)
    }
}
