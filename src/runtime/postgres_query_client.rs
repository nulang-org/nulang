//! PostgreSQL implementation of the worker-owned query protocol.
//!
//! Connections are provided already configured by the caller (including TLS).
//! Construction and network I/O should happen outside actor scheduler threads.

use super::db_executor::{OwnedDbParam, OwnedDbQuery, OwnedDbQueryClient};
use super::persistence::postgres_column_to_json;
use std::io;

use postgres::types::{ToSql, Type};

pub struct PostgresOwnedQueryClient {
    client: postgres::Client,
}

impl PostgresOwnedQueryClient {
    /// Accept an already connected PostgreSQL client. TLS is the caller's
    /// responsibility: this adapter does not create an insecure connection.
    pub fn from_client(client: postgres::Client) -> Self {
        Self { client }
    }
}

/// Bind the exact type that PostgreSQL inferred when preparing a statement.
/// We never cast integer widths implicitly or move unowned VM values into the
/// background worker.
fn bind_owned_param(
    param: &OwnedDbParam,
    expected: &Type,
) -> io::Result<Box<dyn ToSql + Sync>> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "parameter is incompatible with PostgreSQL type {}",
                expected.name()
            ),
        )
    };
    match param {
        OwnedDbParam::Int(value) => {
            if expected == &Type::INT2 {
                Ok(Box::new(i16::try_from(*value).map_err(|_| invalid())?))
            } else if expected == &Type::INT4 {
                Ok(Box::new(i32::try_from(*value).map_err(|_| invalid())?))
            } else if expected == &Type::INT8 {
                Ok(Box::new(*value))
            } else {
                Err(invalid())
            }
        }
        OwnedDbParam::Float(value) => {
            if !value.is_finite() {
                return Err(invalid());
            }
            if expected == &Type::FLOAT4 {
                let narrowed = *value as f32;
                if !narrowed.is_finite() {
                    return Err(invalid());
                }
                Ok(Box::new(narrowed))
            } else if expected == &Type::FLOAT8 {
                Ok(Box::new(*value))
            } else {
                Err(invalid())
            }
        }
        OwnedDbParam::Bool(value) if expected == &Type::BOOL => Ok(Box::new(*value)),
        OwnedDbParam::Text(value)
            if expected == &Type::TEXT
                || expected == &Type::VARCHAR
                || expected == &Type::BPCHAR
                || expected == &Type::NAME
                || expected == &Type::UNKNOWN =>
        {
            Ok(Box::new(value.clone()))
        }
        OwnedDbParam::Json(value) if expected == &Type::JSON || expected == &Type::JSONB => {
            Ok(Box::new(value.clone()))
        }
        OwnedDbParam::Null => {
            if expected == &Type::INT2 {
                Ok(Box::new(None::<i16>))
            } else if expected == &Type::INT4 {
                Ok(Box::new(None::<i32>))
            } else if expected == &Type::INT8 {
                Ok(Box::new(None::<i64>))
            } else if expected == &Type::FLOAT4 {
                Ok(Box::new(None::<f32>))
            } else if expected == &Type::FLOAT8 {
                Ok(Box::new(None::<f64>))
            } else if expected == &Type::BOOL {
                Ok(Box::new(None::<bool>))
            } else if expected == &Type::JSON || expected == &Type::JSONB {
                Ok(Box::new(None::<serde_json::Value>))
            } else if expected == &Type::TEXT
                || expected == &Type::VARCHAR
                || expected == &Type::BPCHAR
                || expected == &Type::NAME
                || expected == &Type::UNKNOWN
            {
                Ok(Box::new(None::<String>))
            } else {
                Err(invalid())
            }
        }
        _ => Err(invalid()),
    }
}

impl OwnedDbQueryClient for PostgresOwnedQueryClient {
    fn execute(&mut self, request: &OwnedDbQuery) -> io::Result<Vec<String>> {
        if request.sql.trim().is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "SQL must not be empty"));
        }
        let statement = self.client.prepare(&request.sql).map_err(|error| {
            io::Error::new(io::ErrorKind::Other, error.to_string())
        })?;
        if statement.params().len() != request.params.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "SQL expects {} parameters, received {}",
                    statement.params().len(),
                    request.params.len()
                ),
            ));
        }
        let params = request
            .params
            .iter()
            .zip(statement.params())
            .map(|(param, expected)| bind_owned_param(param, expected))
            .collect::<io::Result<Vec<_>>>()?;
        let refs = params
            .iter()
            .map(|param| param.as_ref() as &(dyn ToSql + Sync))
            .collect::<Vec<_>>();
        let rows = self.client.query(&statement, &refs).map_err(|error| {
            io::Error::new(io::ErrorKind::Other, error.to_string())
        })?;
        let mut encoded = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut columns = Vec::with_capacity(row.len());
            for index in 0..row.len() {
                columns.push(postgres_column_to_json(row, index)?);
            }
            encoded.push(
                serde_json::to_string(&columns)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
            );
        }
        Ok(encoded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_binding_rejects_out_of_range_values_without_server() {
        assert!(bind_owned_param(&OwnedDbParam::Int(32767), &Type::INT2).is_ok());
        assert_eq!(
            bind_owned_param(&OwnedDbParam::Int(32768), &Type::INT2)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            bind_owned_param(&OwnedDbParam::Int(3_000_000_000), &Type::INT4)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn incompatible_type_and_nan_are_explicit_errors() {
        assert!(bind_owned_param(&OwnedDbParam::Bool(true), &Type::TEXT).is_err());
        assert!(bind_owned_param(&OwnedDbParam::Float(f64::NAN), &Type::FLOAT8).is_err());
        assert!(
            bind_owned_param(&OwnedDbParam::Json(serde_json::json!({"x": 1})), &Type::JSONB)
                .is_ok()
        );
    }

    #[test]
    fn owned_query_executes_against_test_database_when_configured() {
        let Ok(url) = std::env::var("NU_TEST_POSTGRES_URL") else {
            return;
        };
        let client = postgres::Client::connect(&url, postgres::NoTls)
            .expect("dedicated local test PostgreSQL connection");
        let mut backend = PostgresOwnedQueryClient::from_client(client);
        let query = OwnedDbQuery::new(
            "SELECT $1::int2, $2::text, $3::bool, $4::jsonb",
            vec![
                OwnedDbParam::Int(42),
                OwnedDbParam::Text("hello".into()),
                OwnedDbParam::Bool(true),
                OwnedDbParam::Json(serde_json::json!({"x": 1})),
            ],
        );
        let rows = backend.execute(&query).unwrap();
        assert_eq!(rows, vec![r#"[42,"hello",true,{"x":1}]"#.to_owned()]);
    }
}
