//! Paginated results — [`Paginated`].

use serde::Serialize;

/// One page of results, plus enough metadata to render a pager.
///
/// Serialises to the shape an API client expects, so a controller can return
/// it directly:
///
/// ```
/// # use rainier_database::Paginated;
/// let page = Paginated::new(vec!["a", "b"], 25, 2, 10);
///
/// assert_eq!(page.last_page(), 3);
/// assert_eq!(page.from(), Some(11));
/// assert_eq!(page.to(), Some(12));
/// assert!(page.has_more_pages());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Paginated<T> {
    /// The rows on this page.
    pub data: Vec<T>,
    /// How many rows match in total, across every page.
    pub total: u64,
    /// The current page, 1-based.
    pub current_page: u64,
    /// How many rows a full page holds.
    pub per_page: u64,
}

impl<T> Paginated<T> {
    /// Build a page.
    ///
    /// `page` is clamped to at least 1 and `per_page` to at least 1, because a
    /// page zero or a page size of zero has no meaning and every downstream
    /// calculation would divide by it.
    pub fn new(data: Vec<T>, total: u64, page: u64, per_page: u64) -> Self {
        Self { data, total, current_page: page.max(1), per_page: per_page.max(1) }
    }

    /// An empty page.
    pub fn empty(page: u64, per_page: u64) -> Self {
        Self::new(Vec::new(), 0, page, per_page)
    }

    /// The last page number. At least 1, even with no results — "page 1 of 0"
    /// reads worse than "page 1 of 1" holding nothing.
    pub fn last_page(&self) -> u64 {
        self.total.div_ceil(self.per_page).max(1)
    }

    /// The 1-based index of the first row on this page, or `None` if the page
    /// is empty.
    pub fn from(&self) -> Option<u64> {
        if self.data.is_empty() {
            return None;
        }
        Some((self.current_page - 1) * self.per_page + 1)
    }

    /// The 1-based index of the last row on this page.
    pub fn to(&self) -> Option<u64> {
        let from = self.from()?;
        Some(from + self.data.len() as u64 - 1)
    }

    /// Whether another page follows.
    pub fn has_more_pages(&self) -> bool {
        self.current_page < self.last_page()
    }

    /// The next page number, if any.
    pub fn next_page(&self) -> Option<u64> {
        self.has_more_pages().then(|| self.current_page + 1)
    }

    /// The previous page number, if any.
    pub fn previous_page(&self) -> Option<u64> {
        (self.current_page > 1).then(|| self.current_page - 1)
    }

    /// How many rows are on this page.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether this page holds nothing.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// The SQL `OFFSET` this page corresponds to.
    pub fn offset(&self) -> u64 {
        (self.current_page - 1) * self.per_page
    }

    /// Transform each row, keeping the pagination metadata — for turning
    /// models into response resources.
    pub fn map<U>(self, transform: impl FnMut(T) -> U) -> Paginated<U> {
        Paginated {
            data: self.data.into_iter().map(transform).collect(),
            total: self.total,
            current_page: self.current_page,
            per_page: self.per_page,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_the_page_window() {
        let page = Paginated::new(vec![1, 2, 3], 25, 2, 10);
        assert_eq!(page.last_page(), 3);
        assert_eq!(page.offset(), 10);
        assert_eq!(page.from(), Some(11));
        assert_eq!(page.to(), Some(13));
        assert_eq!(page.next_page(), Some(3));
        assert_eq!(page.previous_page(), Some(1));
    }

    #[test]
    fn the_last_page_rounds_up() {
        assert_eq!(Paginated::new(vec![1], 21, 1, 10).last_page(), 3);
        assert_eq!(Paginated::new(vec![1], 20, 1, 10).last_page(), 2);
        assert_eq!(Paginated::new(vec![1], 1, 1, 10).last_page(), 1);
    }

    #[test]
    fn an_empty_result_still_has_a_first_page() {
        let page: Paginated<i32> = Paginated::empty(1, 10);
        assert_eq!(page.last_page(), 1);
        assert_eq!(page.from(), None);
        assert_eq!(page.to(), None);
        assert!(!page.has_more_pages());
        assert!(page.is_empty());
    }

    #[test]
    fn page_and_size_are_clamped_to_sane_values() {
        // A page size of zero would divide by zero in `last_page`.
        let page = Paginated::new(vec![1], 5, 0, 0);
        assert_eq!(page.current_page, 1);
        assert_eq!(page.per_page, 1);
        assert_eq!(page.last_page(), 5);
    }

    #[test]
    fn the_final_page_reports_a_short_window() {
        let page = Paginated::new(vec![1, 2, 3], 23, 3, 10);
        assert_eq!(page.from(), Some(21));
        assert_eq!(page.to(), Some(23));
        assert!(!page.has_more_pages());
        assert_eq!(page.next_page(), None);
    }

    #[test]
    fn map_keeps_the_metadata() {
        let page = Paginated::new(vec![1, 2], 20, 2, 5);
        let mapped = page.map(|n| n.to_string());

        assert_eq!(mapped.data, vec!["1", "2"]);
        assert_eq!(mapped.total, 20);
        assert_eq!(mapped.current_page, 2);
        assert_eq!(mapped.per_page, 5);
    }

    #[test]
    fn serialises_for_an_api_response() {
        let page = Paginated::new(vec!["a"], 3, 1, 2);
        let json = serde_json::to_value(&page).unwrap();

        assert_eq!(json["data"][0], "a");
        assert_eq!(json["total"], 3);
        assert_eq!(json["current_page"], 1);
        assert_eq!(json["per_page"], 2);
    }
}
