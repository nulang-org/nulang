//! Experimental bridge from the Query IR's closed row schema to Nulang's
//! existing structural compiler types. This does not add source-language syntax.
use crate::query::{infer_schema, Catalog, DataType, Plan, QueryError};
use crate::types::{PrimitiveType, Type};

/// Type returned by an in-memory query: [ { field: Type, ... } ].
/// The query is validated before constructing compiler-facing metadata.
pub fn result_collection_type(plan: &Plan, catalog: &Catalog) -> Result<Type, QueryError> {
    let columns = infer_schema(plan, catalog)?;
    let fields = columns
        .into_iter()
        .map(|column| {
            let primitive = match column.ty {
                DataType::Int => PrimitiveType::Int,
                DataType::Bool => PrimitiveType::Bool,
                DataType::Text => PrimitiveType::String,
            };
            (column.name, Type::Primitive(primitive))
        })
        .collect();
    Ok(Type::Array(Box::new(Type::Record(fields))))
}
