use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::{fmt, io};

use fnv::FnvHashMap;
use itertools::{EitherOrBoth, Itertools};

use crate::core::{InvertedIndexReader, Segment, SegmentComponent, SegmentId};
use crate::directory::{CompositeFile, FileSlice};
use crate::error::DataCorruption;
use crate::fastfield::{intersect_alive_bitsets, AliveBitSet, FacetReader, FastFieldReaders};
use crate::fieldnorm::{FieldNormReader, FieldNormReaders};
use crate::json_utils::json_path_sep_to_dot;
use crate::schema::{Field, IndexRecordOption, Schema, Type};
use crate::space_usage::SegmentSpaceUsage;
use crate::store::StoreReader;
use crate::termdict::TermDictionary;
use crate::{DocId, Opstamp};

/// Entry point to access all of the datastructures of the `Segment`
///
/// - term dictionary
/// - postings
/// - store
/// - fast field readers
/// - field norm reader
///
/// The segment reader has a very low memory footprint,
/// as close to all of the memory data is mmapped.
#[derive(Clone)]
pub struct SegmentReader {
    inv_idx_reader_cache: Arc<RwLock<HashMap<Field, Arc<InvertedIndexReader>>>>,

    segment_id: SegmentId,
    delete_opstamp: Option<Opstamp>,

    max_doc: DocId,
    num_docs: DocId,

    termdict_composite: CompositeFile,
    postings_composite: CompositeFile,
    positions_composite: CompositeFile,
    fast_fields_readers: FastFieldReaders,
    fieldnorm_readers: FieldNormReaders,

    store_file: FileSlice,
    alive_bitset_opt: Option<AliveBitSet>,
    schema: Schema,
}

impl SegmentReader {
    /// Returns the highest document id ever attributed in
    /// this segment + 1.
    pub fn max_doc(&self) -> DocId {
        self.max_doc
    }

    /// Returns the number of alive documents.
    /// Deleted documents are not counted.
    pub fn num_docs(&self) -> DocId {
        self.num_docs
    }

    /// Returns the schema of the index this segment belongs to.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Return the number of documents that have been
    /// deleted in the segment.
    pub fn num_deleted_docs(&self) -> DocId {
        self.max_doc - self.num_docs
    }

    /// Returns true if some of the documents of the segment have been deleted.
    pub fn has_deletes(&self) -> bool {
        self.num_deleted_docs() > 0
    }

    /// Accessor to a segment's fast field reader given a field.
    ///
    /// Returns the u64 fast value reader if the field
    /// is a u64 field indexed as "fast".
    ///
    /// Return a FastFieldNotAvailableError if the field is not
    /// declared as a fast field in the schema.
    ///
    /// # Panics
    /// May panic if the index is corrupted.
    pub fn fast_fields(&self) -> &FastFieldReaders {
        &self.fast_fields_readers
    }

    /// Accessor to the `FacetReader` associated with a given `Field`.
    pub fn facet_reader(&self, field_name: &str) -> crate::Result<FacetReader> {
        let schema = self.schema();
        let field = schema.get_field(field_name)?;
        let field_entry = schema.get_field_entry(field);
        if field_entry.field_type().value_type() != Type::Facet {
            return Err(crate::TantivyError::SchemaError(format!(
                "`{field_name}` is not a facet field.`"
            )));
        }
        let Some(facet_column) = self.fast_fields().str(field_name)? else {
            panic!("Facet Field `{field_name}` is missing. This should not happen");
        };
        Ok(FacetReader::new(facet_column))
    }

    /// Accessor to the segment's `Field norms`'s reader.
    ///
    /// Field norms are the length (in tokens) of the fields.
    /// It is used in the computation of the [TfIdf](https://fulmicoton.gitbooks.io/tantivy-doc/content/tfidf.html).
    ///
    /// They are simply stored as a fast field, serialized in
    /// the `.fieldnorm` file of the segment.
    pub fn get_fieldnorms_reader(&self, field: Field) -> crate::Result<FieldNormReader> {
        self.fieldnorm_readers.get_field(field)?.ok_or_else(|| {
            let field_name = self.schema.get_field_name(field);
            let err_msg = format!(
                "Field norm not found for field {field_name:?}. Was the field set to record norm \
                 during indexing?"
            );
            crate::TantivyError::SchemaError(err_msg)
        })
    }

    #[doc(hidden)]
    pub fn fieldnorms_readers(&self) -> &FieldNormReaders {
        &self.fieldnorm_readers
    }

    /// Accessor to the segment's [`StoreReader`](crate::store::StoreReader).
    ///
    /// `cache_num_blocks` sets the number of decompressed blocks to be cached in an LRU.
    /// The size of blocks is configurable, this should be reflexted in the
    pub fn get_store_reader(&self, cache_num_blocks: usize) -> io::Result<StoreReader> {
        StoreReader::open(self.store_file.clone(), cache_num_blocks)
    }

    /// Open a new segment for reading.
    pub fn open(segment: &Segment) -> crate::Result<SegmentReader> {
        Self::open_with_custom_alive_set(segment, None)
    }

    /// Open a new segment for reading.
    pub fn open_with_custom_alive_set(
        segment: &Segment,
        custom_bitset: Option<AliveBitSet>,
    ) -> crate::Result<SegmentReader> {
        let termdict_file = segment.open_read(SegmentComponent::Terms)?;
        let termdict_composite = CompositeFile::open(&termdict_file)?;

        let store_file = segment.open_read(SegmentComponent::Store)?;

        crate::fail_point!("SegmentReader::open#middle");

        let postings_file = segment.open_read(SegmentComponent::Postings)?;
        let postings_composite = CompositeFile::open(&postings_file)?;

        let positions_composite = {
            if let Ok(positions_file) = segment.open_read(SegmentComponent::Positions) {
                CompositeFile::open(&positions_file)?
            } else {
                CompositeFile::empty()
            }
        };

        let schema = segment.schema();

        let fast_fields_data = segment.open_read(SegmentComponent::FastFields)?;
        let fast_fields_readers = FastFieldReaders::open(fast_fields_data, schema.clone())?;
        let fieldnorm_data = segment.open_read(SegmentComponent::FieldNorms)?;
        let fieldnorm_readers = FieldNormReaders::open(fieldnorm_data)?;

        let original_bitset = if segment.meta().has_deletes() {
            let alive_doc_file_slice = segment.open_read(SegmentComponent::Delete)?;
            let alive_doc_data = alive_doc_file_slice.read_bytes()?;
            Some(AliveBitSet::open(alive_doc_data))
        } else {
            None
        };

        let alive_bitset_opt = intersect_alive_bitset(original_bitset, custom_bitset);

        let max_doc = segment.meta().max_doc();
        let num_docs = alive_bitset_opt
            .as_ref()
            .map(|alive_bitset| alive_bitset.num_alive_docs() as u32)
            .unwrap_or(max_doc);

        Ok(SegmentReader {
            inv_idx_reader_cache: Default::default(),
            num_docs,
            max_doc,
            termdict_composite,
            postings_composite,
            fast_fields_readers,
            fieldnorm_readers,
            segment_id: segment.id(),
            delete_opstamp: segment.meta().delete_opstamp(),
            store_file,
            alive_bitset_opt,
            positions_composite,
            schema,
        })
    }

    /// Returns a field reader associated with the field given in argument.
    /// If the field was not present in the index during indexing time,
    /// the InvertedIndexReader is empty.
    ///
    /// The field reader is in charge of iterating through the
    /// term dictionary associated with a specific field,
    /// and opening the posting list associated with any term.
    ///
    /// If the field is not marked as index, a warning is logged and an empty `InvertedIndexReader`
    /// is returned.
    /// Similarly, if the field is marked as indexed but no term has been indexed for the given
    /// index, an empty `InvertedIndexReader` is returned (but no warning is logged).
    pub fn inverted_index(&self, field: Field) -> crate::Result<Arc<InvertedIndexReader>> {
        if let Some(inv_idx_reader) = self
            .inv_idx_reader_cache
            .read()
            .expect("Lock poisoned. This should never happen")
            .get(&field)
        {
            return Ok(Arc::clone(inv_idx_reader));
        }
        let field_entry = self.schema.get_field_entry(field);
        let field_type = field_entry.field_type();
        let record_option_opt = field_type.get_index_record_option();

        if record_option_opt.is_none() {
            warn!("Field {:?} does not seem indexed.", field_entry.name());
        }

        let postings_file_opt = self.postings_composite.open_read(field);

        if postings_file_opt.is_none() || record_option_opt.is_none() {
            // no documents in the segment contained this field.
            // As a result, no data is associated with the inverted index.
            //
            // Returns an empty inverted index.
            let record_option = record_option_opt.unwrap_or(IndexRecordOption::Basic);
            return Ok(Arc::new(InvertedIndexReader::empty(record_option)));
        }

        let record_option = record_option_opt.unwrap();
        let postings_file = postings_file_opt.unwrap();

        let termdict_file: FileSlice =
            self.termdict_composite.open_read(field).ok_or_else(|| {
                DataCorruption::comment_only(format!(
                    "Failed to open field {:?}'s term dictionary in the composite file. Has the \
                     schema been modified?",
                    field_entry.name()
                ))
            })?;

        let positions_file = self.positions_composite.open_read(field).ok_or_else(|| {
            let error_msg = format!(
                "Failed to open field {:?}'s positions in the composite file. Has the schema been \
                 modified?",
                field_entry.name()
            );
            DataCorruption::comment_only(error_msg)
        })?;

        let inv_idx_reader = Arc::new(InvertedIndexReader::new(
            TermDictionary::open(termdict_file)?,
            postings_file,
            positions_file,
            record_option,
        )?);

        // by releasing the lock in between, we may end up opening the inverting index
        // twice, but this is fine.
        self.inv_idx_reader_cache
            .write()
            .expect("Field reader cache lock poisoned. This should never happen.")
            .insert(field, Arc::clone(&inv_idx_reader));

        Ok(inv_idx_reader)
    }

    /// Returns the list of fields that have been indexed in the segment.
    /// The field list includes the field defined in the schema as well as the fields
    /// that have been indexed as a part of a JSON field.
    /// The returned field name is the full field name, including the name of the JSON field.
    ///
    /// The returned field names can be used in queries.
    ///
    /// Notice: If your data contains JSON fields this is **very expensive**.
    pub fn fields_metadata(&self) -> crate::Result<Vec<FieldMetadata>> {
        let mut indexed_fields: Vec<(String, Type)> = Vec::new();
        let mut map_to_canonical = FnvHashMap::default();
        for (field, field_entry) in self.schema().fields() {
            let field_name = field_entry.name().to_string();
            let is_indexed = field_entry.is_indexed();

            if is_indexed {
                indexed_fields.push((
                    field_name.to_string(),
                    field_entry.field_type().value_type(),
                ));
                let is_json = field_entry.field_type().value_type() == Type::Json;
                if is_json {
                    let inv_index = self.inverted_index(field)?;
                    let encoded_fields_in_index = inv_index.list_encoded_fields()?;
                    let mut build_path = |field_name: &str, mut json_path: String| {
                        // In this case we need to map the potential fast field to the field name
                        // accepted by the query parser.
                        let create_canonical =
                            !field_entry.is_expand_dots_enabled() && json_path.contains('.');
                        if create_canonical {
                            // Without expand dots enabled dots need to be escaped.
                            let escaped_json_path = json_path.replace('.', "\\.");
                            let full_path = format!("{}.{}", field_name, escaped_json_path);
                            let full_path_unescaped = format!("{}.{}", field_name, &json_path);
                            map_to_canonical.insert(full_path_unescaped, full_path.to_string());
                            full_path
                        } else {
                            // With expand dots enabled, we can use '.' instead of '\u{1}'.
                            json_path_sep_to_dot(&mut json_path);
                            format!("{}.{}", field_name, json_path)
                        }
                    };
                    indexed_fields.extend(
                        encoded_fields_in_index
                            .into_iter()
                            .map(|(name, typ)| (build_path(&field_name, name), typ)),
                    );
                }
            }
        }
        let mut fast_fields: Vec<(String, Type)> = self
            .fast_fields()
            .columnar()
            .iter_columns()?
            .map(|(mut field_name, handle)| {
                json_path_sep_to_dot(&mut field_name);
                // map to canonical path, to avoid similar but different entries.
                // Eventually we should just accept '.' seperated for all cases.
                let field_name = map_to_canonical
                    .get(&field_name)
                    .unwrap_or(&field_name)
                    .to_string();

                (field_name, Type::from(handle.column_type()))
            })
            .collect();
        // Since the type is encoded differently in the fast field and in the inverted index,
        // the order of the fields is not guaranteed to be the same. Therefore, we sort the fields.
        // If we are sure that the order is the same, we can remove this sort.
        indexed_fields.sort_unstable();
        fast_fields.sort_unstable();

        // Maybe too slow for the high cardinality case
        fn is_field_stored(field_name: &str, schema: &Schema) -> bool {
            schema
                .find_field(field_name)
                .map(|(field, _path)| schema.get_field_entry(field).is_stored())
                .unwrap_or(false)
        }

        let merged: Vec<FieldMetadata> = indexed_fields
            .iter()
            .merge_join_by(fast_fields.iter(), |i, j| i.cmp(j))
            .map(|either| match either {
                EitherOrBoth::Left((field_name, typ)) => FieldMetadata {
                    field_name: field_name.clone(),
                    indexed: true,
                    stored: is_field_stored(field_name, self.schema()),
                    fast: false,
                    typ: *typ,
                },
                EitherOrBoth::Right((field_name, typ)) => FieldMetadata {
                    field_name: field_name.clone(),
                    indexed: false,
                    stored: is_field_stored(field_name, self.schema()),
                    fast: true,
                    typ: *typ,
                },
                EitherOrBoth::Both((field_name, typ), (_field_name2, _typ2)) => FieldMetadata {
                    field_name: field_name.clone(),
                    indexed: true,
                    stored: is_field_stored(field_name, self.schema()),
                    fast: true,
                    typ: *typ,
                },
            })
            .collect();

        Ok(merged)
    }

    /// Returns the segment id
    pub fn segment_id(&self) -> SegmentId {
        self.segment_id
    }

    /// Returns the delete opstamp
    pub fn delete_opstamp(&self) -> Option<Opstamp> {
        self.delete_opstamp
    }

    /// Returns the bitset representing the alive `DocId`s.
    pub fn alive_bitset(&self) -> Option<&AliveBitSet> {
        self.alive_bitset_opt.as_ref()
    }

    /// Returns true if the `doc` is marked
    /// as deleted.
    pub fn is_deleted(&self, doc: DocId) -> bool {
        self.alive_bitset()
            .map(|alive_bitset| alive_bitset.is_deleted(doc))
            .unwrap_or(false)
    }

    /// Returns an iterator that will iterate over the alive document ids
    pub fn doc_ids_alive(&self) -> Box<dyn Iterator<Item = DocId> + '_> {
        if let Some(alive_bitset) = &self.alive_bitset_opt {
            Box::new(alive_bitset.iter_alive())
        } else {
            Box::new(0u32..self.max_doc)
        }
    }

    /// Summarize total space usage of this segment.
    pub fn space_usage(&self) -> io::Result<SegmentSpaceUsage> {
        Ok(SegmentSpaceUsage::new(
            self.num_docs(),
            self.termdict_composite.space_usage(),
            self.postings_composite.space_usage(),
            self.positions_composite.space_usage(),
            self.fast_fields_readers.space_usage(self.schema())?,
            self.fieldnorm_readers.space_usage(),
            self.get_store_reader(0)?.space_usage(),
            self.alive_bitset_opt
                .as_ref()
                .map(AliveBitSet::space_usage)
                .unwrap_or_default(),
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
/// FieldMetadata
pub struct FieldMetadata {
    /// The field name
    pub field_name: String,
    /// Is the field indexed for search
    pub indexed: bool,
    /// Is the field stored in the doc store
    pub stored: bool,
    /// Is the field stored in the columnar storage
    pub fast: bool,
    /// The field type
    pub typ: Type,
}

fn intersect_alive_bitset(
    left_opt: Option<AliveBitSet>,
    right_opt: Option<AliveBitSet>,
) -> Option<AliveBitSet> {
    match (left_opt, right_opt) {
        (Some(left), Some(right)) => {
            assert_eq!(left.bitset().max_value(), right.bitset().max_value());
            Some(intersect_alive_bitsets(left, right))
        }
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

impl fmt::Debug for SegmentReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SegmentReader({:?})", self.segment_id)
    }
}

#[cfg(test)]
mod test {
    use crate::core::Index;
    use crate::schema::{Schema, Term, STORED, TEXT};
    use crate::{DocId, IndexWriter};

    #[test]
    fn test_num_alive() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field("name", TEXT | STORED);
        let schema = schema_builder.build();
        let index = Index::create_in_ram(schema.clone());
        let name = schema.get_field("name").unwrap();

        {
            let mut index_writer: IndexWriter = index.writer_for_tests()?;
            index_writer.add_document(doc!(name => "tantivy"))?;
            index_writer.add_document(doc!(name => "horse"))?;
            index_writer.add_document(doc!(name => "jockey"))?;
            index_writer.add_document(doc!(name => "cap"))?;
            // we should now have one segment with two docs
            index_writer.delete_term(Term::from_field_text(name, "horse"));
            index_writer.delete_term(Term::from_field_text(name, "cap"));

            // ok, now we should have a deleted doc
            index_writer.commit()?;
        }
        let searcher = index.reader()?.searcher();
        assert_eq!(2, searcher.segment_reader(0).num_docs());
        assert_eq!(4, searcher.segment_reader(0).max_doc());
        Ok(())
    }
    #[test]
    fn test_alive_docs_iterator() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field("name", TEXT | STORED);
        let schema = schema_builder.build();
        let index = Index::create_in_ram(schema.clone());
        let name = schema.get_field("name").unwrap();

        {
            let mut index_writer: IndexWriter = index.writer_for_tests()?;
            index_writer.add_document(doc!(name => "tantivy"))?;
            index_writer.add_document(doc!(name => "horse"))?;
            index_writer.add_document(doc!(name => "jockey"))?;
            index_writer.add_document(doc!(name => "cap"))?;
            // we should now have one segment with two docs
            index_writer.commit()?;
        }

        {
            let mut index_writer2: IndexWriter = index.writer(50_000_000)?;
            index_writer2.delete_term(Term::from_field_text(name, "horse"));
            index_writer2.delete_term(Term::from_field_text(name, "cap"));

            // ok, now we should have a deleted doc
            index_writer2.commit()?;
        }
        let searcher = index.reader()?.searcher();
        let docs: Vec<DocId> = searcher.segment_reader(0).doc_ids_alive().collect();
        assert_eq!(vec![0u32, 2u32], docs);
        Ok(())
    }
}
