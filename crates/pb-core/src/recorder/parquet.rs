//! Parquet export of telemetry channels: `time_s` plus one nullable
//! `float64` column per channel, as-of joined like the joined CSV, Snappy
//! compressed. Available with the default `parquet` feature.

use std::path::Path;

use crate::error::CoreError;
use crate::telemetry::TelemetryStore;

/// Rows per record batch handed to the writer.
#[cfg(feature = "parquet")]
const BATCH_ROWS: usize = 65_536;

/// Exports `channels` (all when empty) within `range` (seconds, inclusive).
/// Returns the number of rows written.
#[cfg(feature = "parquet")]
pub fn export_parquet(
    store: &TelemetryStore,
    channels: &[String],
    range: Option<(f64, f64)>,
    path: &Path,
) -> Result<usize, CoreError> {
    use std::sync::Arc;

    use arrow_array::builder::Float64Builder;
    use arrow_array::{ArrayRef, RecordBatch};
    use arrow_schema::{DataType, Field, Schema};
    use parquet::arrow::ArrowWriter;
    use parquet::basic::Compression;
    use parquet::file::properties::WriterProperties;

    let err = |e: &dyn std::fmt::Display| CoreError::Recorder(format!("parquet: {e}"));
    let (chans, samples) = super::join::select(store, channels, range)?;
    let mut fields = vec![Field::new("time_s", DataType::Float64, false)];
    fields.extend(
        chans
            .iter()
            .map(|c| Field::new(c.name.clone(), DataType::Float64, true)),
    );
    let schema = Arc::new(Schema::new(fields));
    let file = std::fs::File::create(path).map_err(|e| CoreError::io(path, e))?;
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut w = ArrowWriter::try_new(file, schema.clone(), Some(props)).map_err(|e| err(&e))?;

    let mut builders: Vec<Float64Builder> = (0..=chans.len())
        .map(|_| Float64Builder::with_capacity(BATCH_ROWS))
        .collect();
    let flush = |w: &mut ArrowWriter<std::fs::File>, builders: &mut [Float64Builder]| {
        let cols: Vec<ArrayRef> = builders
            .iter_mut()
            .map(|b| Arc::new(b.finish()) as ArrayRef)
            .collect();
        let batch = RecordBatch::try_new(schema.clone(), cols).map_err(|e| err(&e))?;
        w.write(&batch).map_err(|e| err(&e))
    };
    let mut rows = super::join::AsOf::new(&samples);
    let mut total = 0usize;
    let mut in_batch = 0usize;
    while let Some(t) = rows.next_row() {
        builders[0].append_value(t);
        for (b, v) in builders[1..].iter_mut().zip(rows.values()) {
            b.append_option(*v);
        }
        total += 1;
        in_batch += 1;
        if in_batch == BATCH_ROWS {
            flush(&mut w, &mut builders)?;
            in_batch = 0;
        }
    }
    if in_batch > 0 || total == 0 {
        flush(&mut w, &mut builders)?;
    }
    w.close().map_err(|e| err(&e))?;
    Ok(total)
}

/// Built without the `parquet` feature.
#[cfg(not(feature = "parquet"))]
pub fn export_parquet(
    _store: &TelemetryStore,
    _channels: &[String],
    _range: Option<(f64, f64)>,
    _path: &Path,
) -> Result<usize, CoreError> {
    Err(CoreError::Recorder(
        "this build has no Parquet support (pb-core feature `parquet`)".into(),
    ))
}

#[cfg(all(test, feature = "parquet"))]
mod tests {
    use super::*;
    use crate::schema::FieldDisplay;
    use crate::telemetry::ChannelKind;
    use arrow_array::{Array, Float64Array};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    fn read(path: &Path) -> (Vec<String>, Vec<Vec<Option<f64>>>) {
        let f = std::fs::File::open(path).unwrap();
        let reader = ParquetRecordBatchReaderBuilder::try_new(f)
            .unwrap()
            .build()
            .unwrap();
        let mut names = Vec::new();
        let mut cols: Vec<Vec<Option<f64>>> = Vec::new();
        for batch in reader {
            let batch = batch.unwrap();
            if names.is_empty() {
                names = batch
                    .schema()
                    .fields()
                    .iter()
                    .map(|f| f.name().clone())
                    .collect();
                cols = vec![Vec::new(); names.len()];
            }
            for (i, c) in batch.columns().iter().enumerate() {
                let a = c.as_any().downcast_ref::<Float64Array>().unwrap();
                cols[i].extend((0..a.len()).map(|j| (!a.is_null(j)).then(|| a.value(j))));
            }
        }
        (names, cols)
    }

    #[test]
    fn joined_columns_round_trip() {
        let s = TelemetryStore::new(1_000_000);
        let a = s.get_or_create("a", || (FieldDisplay::plain("a"), ChannelKind::Number));
        let b = s.get_or_create("b.x[0]", || (FieldDisplay::plain("b"), ChannelKind::Number));
        a.push(1.0, 10.0);
        a.push(3.0, 30.0);
        b.push(2.0, 2.5);
        b.push(3.0, 3.5);
        let p = std::env::temp_dir().join(format!("pb-pq-{}.parquet", std::process::id()));
        assert_eq!(export_parquet(&s, &[], None, &p).unwrap(), 3);
        let (names, cols) = read(&p);
        assert_eq!(names, vec!["time_s", "a", "b.x[0]"]);
        assert_eq!(cols[0], vec![Some(1.0), Some(2.0), Some(3.0)]);
        assert_eq!(cols[1], vec![Some(10.0), Some(10.0), Some(30.0)]);
        assert_eq!(cols[2], vec![None, Some(2.5), Some(3.5)]);
        // Range and channel selection.
        assert_eq!(
            export_parquet(&s, &["b.x[0]".into()], Some((2.5, 9.0)), &p).unwrap(),
            1
        );
        let (names, cols) = read(&p);
        assert_eq!(names, vec!["time_s", "b.x[0]"]);
        assert_eq!(cols[1], vec![Some(3.5)]);
        assert!(export_parquet(&s, &["zz".into()], None, &p).is_err());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn many_rows_span_batches() {
        let s = TelemetryStore::new(1_000_000);
        let a = s.get_or_create("a", || (FieldDisplay::plain("a"), ChannelKind::Number));
        let n = BATCH_ROWS * 2 + 17;
        for i in 0..n {
            a.push(i as f64 * 1e-3, i as f64);
        }
        let p = std::env::temp_dir().join(format!("pb-pq-big-{}.parquet", std::process::id()));
        assert_eq!(export_parquet(&s, &[], None, &p).unwrap(), n);
        let (_, cols) = read(&p);
        assert_eq!(cols[1].len(), n);
        assert_eq!(cols[1][n - 1], Some((n - 1) as f64));
        let _ = std::fs::remove_file(p);
    }
}
