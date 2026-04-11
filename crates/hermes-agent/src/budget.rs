//! Iteration budget — limits how many agent loop turns can execute.

/// Tracks the number of remaining agent loop iterations.
///
/// `try_consume` decrements the counter and returns `false` when exhausted.
/// `refund` adds back iterations (saturating, capped at `max`).
#[derive(Debug, Clone)]
pub struct IterationBudget {
    remaining: u32,
    max: u32,
}

impl IterationBudget {
    /// Create a new budget with `max` allowed iterations.
    pub fn new(max: u32) -> Self {
        Self {
            remaining: max,
            max,
        }
    }

    /// Try to consume one iteration.
    ///
    /// Returns `true` if an iteration was consumed, `false` if the budget is exhausted.
    pub fn try_consume(&mut self) -> bool {
        if self.remaining == 0 {
            return false;
        }
        self.remaining -= 1;
        true
    }

    /// Refund `n` iterations, saturating at `max`.
    pub fn refund(&mut self, n: u32) {
        self.remaining = self.remaining.saturating_add(n).min(self.max);
    }

    /// Number of iterations remaining.
    pub fn remaining(&self) -> u32 {
        self.remaining
    }

    /// Maximum allowed iterations.
    pub fn max(&self) -> u32 {
        self.max
    }

    /// Returns `true` when no iterations remain.
    pub fn is_exhausted(&self) -> bool {
        self.remaining == 0
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_budget() {
        let b = IterationBudget::new(10);
        assert_eq!(b.remaining(), 10);
        assert_eq!(b.max(), 10);
        assert!(!b.is_exhausted());
    }

    #[test]
    fn consume_decrements() {
        let mut b = IterationBudget::new(5);
        assert!(b.try_consume());
        assert_eq!(b.remaining(), 4);
        assert!(b.try_consume());
        assert_eq!(b.remaining(), 3);
    }

    #[test]
    fn consume_returns_false_when_exhausted() {
        let mut b = IterationBudget::new(1);
        assert!(b.try_consume());
        assert!(!b.try_consume());
        assert_eq!(b.remaining(), 0);
        assert!(b.is_exhausted());
    }

    #[test]
    fn refund() {
        let mut b = IterationBudget::new(10);
        b.try_consume();
        b.try_consume();
        assert_eq!(b.remaining(), 8);
        b.refund(1);
        assert_eq!(b.remaining(), 9);
    }

    #[test]
    fn refund_capped_at_max() {
        let mut b = IterationBudget::new(5);
        b.try_consume();
        // remaining = 4; refund 100 should cap at max=5
        b.refund(100);
        assert_eq!(b.remaining(), 5);
        assert_eq!(b.remaining(), b.max());
    }

    #[test]
    fn zero_budget() {
        let mut b = IterationBudget::new(0);
        assert!(b.is_exhausted());
        assert!(!b.try_consume());
        assert_eq!(b.remaining(), 0);
    }

    #[test]
    fn refund_from_zero() {
        let mut b = IterationBudget::new(3);
        // exhaust completely
        while b.try_consume() {}
        assert!(b.is_exhausted());
        b.refund(2);
        assert_eq!(b.remaining(), 2);
        assert!(!b.is_exhausted());
    }
}
