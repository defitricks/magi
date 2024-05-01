//! Defines an [Iterator] that can purge itself.

/// Iterator that can purge itself
pub trait PurgeableIterator: Iterator {
    /// Purges and resets an iterator
    fn purge(&mut self);
}
