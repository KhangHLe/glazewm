use wm_platform::Direction;

pub enum WorkspaceTarget {
  Name(String),
  /// Workspace matched by name or display name among the workspaces on
  /// the origin workspace's monitor (Komorebi-style per-monitor
  /// workspace addressing).
  NameOnMonitor(String),
  Recent,
  NextActive,
  PreviousActive,
  NextActiveInMonitor,
  PreviousActiveInMonitor,
  Next,
  Previous,
  #[allow(dead_code)]
  Direction(Direction),
}
