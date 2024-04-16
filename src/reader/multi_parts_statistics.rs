use std::collections::HashMap;

use crate::{schema::Field, Term};

/// A structure for storing statistics related to multiple parts of a document collection.
/// 
/// MyScale creates a separate tantivy index for each data part of a table. 
/// MultiPartsStatistics allows MyScale to accurately calculate bm25 scores across multiple data parts.
#[derive(Debug, Clone)]
pub struct MultiPartsStatistics {
    /// Maps each term to its document frequency (number of documents containing the term)
    pub doc_freq_map: HashMap<Term, u64>,
    /// Maps earch field to the total number of tokens within that field.
    pub total_num_tokens: HashMap<Field, u64>,
    /// Records total number of documents in the collection.
    pub total_num_docs: u64,
}
impl MultiPartsStatistics {
    /// Constructs a new `MultiPartsStatistics`.
    ///
    /// # Parameters
    /// - `doc_freq_map`: A mapping from terms to their document frequencies.
    /// - `total_num_tokens`: A mapping from fields to their total token counts.
    /// - `total_num_docs`: The total number of documents in the statistics.
    ///
    /// # Returns
    /// A new instance of `MultiPartsStatistics`.
    pub fn new(doc_freq_map: HashMap<Term, u64>, total_num_tokens: HashMap<Field, u64>, total_num_docs: u64) -> Self {
        Self {
            doc_freq_map,
            total_num_tokens,
            total_num_docs,
        }
    }

    /// Returns the document frequency of a specific term.
    ///
    /// # Parameters
    /// - `term`: A reference to the term whose document frequency is being queried.
    ///
    /// # Returns
    /// The document frequency of the given term. Returns `0` if the term is not found.
    pub fn doc_freq(&self, term: &Term) -> u64 {
        let res = self.doc_freq_map.get(term);
        res.unwrap_or(&0).clone()
    }

    /// Retrieves the total number of tokens for a specified field.
    ///
    /// # Parameters
    /// - `field`: A reference to the field for which the token count is needed.
    ///
    /// # Panics
    /// This method will panic if the field does not exist in the map.
    ///
    /// # Returns
    /// The total number of tokens for the field.
    pub fn total_num_tokens(&self, field: &Field) -> u64 {
        let nums = self.total_num_tokens.get(field);
        // assert!(nums.is_some(), "Bm25 requires total_num_tokens not none for field:{:?}", field);
        if nums.is_some() {
            nums.unwrap().clone()
        } else {
            0u64
        }
    }

    /// Returns the total number of documents in the collection.
    ///
    /// # Returns
    /// The total number of documents as a `u64`.
    pub fn total_num_docs(&self) -> u64 {
        self.total_num_docs
    }
}

