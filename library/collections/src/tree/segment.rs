//! A module providing a Dynamic Segment Tree data structure for efficient range
//! queries and updates. It supports lazy propagation and dynamic node
//! allocation, making it suitable for managing huge, sparse domains like
//! virtual memory spaces.

use alloc::boxed::Box;
use core::{fmt::Debug, ops::Range};

/// Custom error types representing possible errors that can occur during
/// operations on the `SegmentTree`.
#[derive(Debug, PartialEq, Eq)]
pub enum SegmentTreeError {
    /// Error indicating that an index or range is out of bounds.
    IndexOutOfBounds,
    /// Error indicating that a range provided for a query is invalid.
    InvalidRange,
}

pub trait SegmentState: Clone + Default + PartialEq {
    fn merge(left: &Self, right: &Self) -> Self;
    fn apply(&mut self, lazy_value: &Self, len: usize);
}

#[derive(Debug, Clone)]
struct Node<T: SegmentState> {
    val: T,
    lazy: Option<T>,
    left: Option<Box<Node<T>>>,
    right: Option<Box<Node<T>>>,
}

impl<T: SegmentState> Node<T> {
    fn new(val: T) -> Self {
        Self {
            val,
            lazy: None,
            left: None,
            right: None,
        }
    }
}

pub struct SegmentTree<T: SegmentState> {
    root: Option<Box<Node<T>>>,
    domain: Range<usize>,
}

impl<T: SegmentState> SegmentTree<T> {
    pub fn new(domain: Range<usize>) -> Self {
        Self { root: None, domain }
    }

    pub fn update(&mut self, range: Range<usize>, val: T) -> Result<(), SegmentTreeError> {
        if range.start >= range.end
            || range.start < self.domain.start
            || range.end > self.domain.end
        {
            return Err(SegmentTreeError::InvalidRange);
        }

        let mut root = self
            .root
            .take()
            .unwrap_or_else(|| Box::new(Node::new(T::default())));
        self.update_rec(
            &mut root,
            self.domain.start,
            self.domain.end,
            range.start,
            range.end,
            &val,
        );
        self.root = Some(root);

        Ok(())
    }

    fn update_rec(
        &self,
        node: &mut Box<Node<T>>,
        l: usize,
        r: usize,
        ql: usize,
        qr: usize,
        val: &T,
    ) {
        // 核心优化：如果当前区间被目标区间完全覆盖，直接打标记并【剪枝】
        if ql <= l && r <= qr {
            node.val.apply(val, r - l);
            node.lazy = Some(val.clone());
            // 内存回收：子节点的状态被彻底覆盖，不再需要保留，直接 Drop 释放内存！
            node.left = None;
            node.right = None;
            return;
        }

        self.push_down(node, l, r);
        let mid = l + (r - l) / 2;

        if ql < mid {
            if node.left.is_none() {
                node.left = Some(Box::new(Node::new(T::default())));
            }
            self.update_rec(node.left.as_mut().unwrap(), l, mid, ql, qr, val);
        }
        if qr > mid {
            if node.right.is_none() {
                node.right = Some(Box::new(Node::new(T::default())));
            }
            self.update_rec(node.right.as_mut().unwrap(), mid, r, ql, qr, val);
        }

        self.push_up(node);
    }

    pub fn query(&mut self, range: Range<usize>) -> Result<T, SegmentTreeError> {
        if range.start >= range.end
            || range.start < self.domain.start
            || range.end > self.domain.end
        {
            return Err(SegmentTreeError::InvalidRange);
        }

        if self.root.is_none() {
            return Ok(T::default());
        }

        let mut root = self.root.take().unwrap();
        let res = self.query_rec(
            &mut root,
            self.domain.start,
            self.domain.end,
            range.start,
            range.end,
        );
        self.root = Some(root);

        Ok(res)
    }

    fn query_rec(&self, node: &mut Box<Node<T>>, l: usize, r: usize, ql: usize, qr: usize) -> T {
        if ql <= l && r <= qr {
            return node.val.clone();
        }

        self.push_down(node, l, r);
        let mid = l + (r - l) / 2;
        let mut res_left = None;
        let mut res_right = None;

        if ql < mid {
            if let Some(left) = &mut node.left {
                res_left = Some(self.query_rec(left, l, mid, ql, qr));
            } else {
                res_left = Some(T::default());
            }
        }
        if qr > mid {
            if let Some(right) = &mut node.right {
                res_right = Some(self.query_rec(right, mid, r, ql, qr));
            } else {
                res_right = Some(T::default());
            }
        }

        match (res_left, res_right) {
            (Some(vl), Some(vr)) => T::merge(&vl, &vr),
            (Some(vl), None) => vl,
            (None, Some(vr)) => vr,
            (None, None) => unreachable!(),
        }
    }

    fn push_down(&self, node: &mut Box<Node<T>>, l: usize, r: usize) {
        if let Some(lazy_val) = node.lazy.take() {
            let mid = l + (r - l) / 2;

            if node.left.is_none() {
                node.left = Some(Box::new(Node::new(T::default())));
            }
            let left = node.left.as_mut().unwrap();
            left.val.apply(&lazy_val, mid - l);
            left.lazy = Some(lazy_val.clone());
            left.left = None;
            left.right = None;

            if node.right.is_none() {
                node.right = Some(Box::new(Node::new(T::default())));
            }
            let right = node.right.as_mut().unwrap();
            right.val.apply(&lazy_val, r - mid);
            right.lazy = Some(lazy_val);
            right.left = None;
            right.right = None;
        }
    }

    fn push_up(&self, node: &mut Box<Node<T>>) {
        let left_val = node
            .left
            .as_ref()
            .map(|n| &n.val)
            .cloned()
            .unwrap_or_else(T::default);
        let right_val = node
            .right
            .as_ref()
            .map(|n| &n.val)
            .cloned()
            .unwrap_or_else(T::default);
        node.val = T::merge(&left_val, &right_val);
    }
}
