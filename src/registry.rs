//! Widget Registry — central namespace for tracking UI widgets by name and type.
//!
//! Supports insertion, lookup by name, and iteration over all registered widgets.
//! This is the foundation that later stages (heartbeat listeners, guard display,
//! progress bar, verdict banner) hang off.

use std::collections::HashMap;

/// The kind of visual element a widget represents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WidgetType {
    ProgressBar,
    Table,
    Banner,
    Heartbeat,
    Tree,
}

/// A single registered widget.
#[derive(Debug, Clone)]
pub struct Widget {
    pub name: String,
    pub wtype: WidgetType,
    pub enabled: bool,
}

/// Central registry that tracks all UI widgets by name and type.
pub struct WidgetRegistry {
    widgets: HashMap<String, Widget>,
    /// Insertion order preserved for deterministic iteration.
    order: Vec<String>,
}

impl WidgetRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            widgets: HashMap::new(),
            order: Vec::new(),
        }
    }

    /// Insert a widget. If a widget with the same name exists, it is replaced
    /// (its position in iteration order is preserved).
    pub fn insert(&mut self, name: impl Into<String>, wtype: WidgetType) {
        let name = name.into();
        let widget = Widget {
            name: name.clone(),
            wtype,
            enabled: true,
        };
        if self.widgets.insert(name.clone(), widget).is_none() {
            self.order.push(name);
        }
    }

    /// Look up a widget by name.
    pub fn get(&self, name: &str) -> Option<&Widget> {
        self.widgets.get(name)
    }

    /// Iterate over all widgets in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &Widget> {
        self.order.iter().filter_map(|n| self.widgets.get(n))
    }

    /// Number of registered widgets.
    pub fn len(&self) -> usize {
        self.widgets.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.widgets.is_empty()
    }

    /// Remove a widget by name. Returns the removed widget, if any.
    pub fn remove(&mut self, name: &str) -> Option<Widget> {
        if let Some(w) = self.widgets.remove(name) {
            self.order.retain(|n| n != name);
            Some(w)
        } else {
            None
        }
    }
}
