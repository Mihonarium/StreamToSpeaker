//! Only Windows' current render endpoints tell us whether the output is usable.

use super::World;
use crate::model::scene::RepairNow;
use crate::platform::RepairExit;

pub(super) enum RepairPlayback {
    Idle,
    /// The output can appear while the helper is still waiting for approval.
    Helper { run: u64, until: u64 },
    WhenFinished,
    AgainAt(u64),
}

impl RepairPlayback {
    pub(super) fn due(&self) -> Option<u64> {
        match *self {
            Self::Helper { until, .. } | Self::AgainAt(until) => Some(until),
            _ => None,
        }
    }
}

#[derive(Default, PartialEq, Eq)]
pub(super) enum Recovery {
    #[default]
    Idle,
    TurningOn { until: u64 },
    TurnOnFailed,
    Asking { run: u64, until: u64 },
    Running { run: u64, until: u64 },
    WaitingForOutput { until: u64 },
    NotApproved,
    Failed,
    Worked,
}

impl Recovery {
    pub(super) fn repairing(&self) -> bool {
        matches!(self, Self::Asking { .. } | Self::Running { .. } | Self::WaitingForOutput { .. })
    }

    pub(super) fn changing_output(&self) -> bool {
        matches!(self, Self::TurningOn { .. } | Self::Running { .. } | Self::WaitingForOutput { .. })
    }

    pub(super) fn turn_on(now: u64) -> Self {
        Self::TurningOn { until: now + 15_000 }
    }

    pub(super) fn observe_approval(&mut self, w: &World, now: u64) {
        if let Self::Asking { until, .. } = *self {
            if w.our_device {
                *self = Self::Worked;
            } else if now >= until {
                *self = Self::Idle;
            }
        }
    }

    pub(super) fn due(&self) -> Option<u64> {
        match *self {
            Self::TurningOn { until } | Self::Asking { until, .. } | Self::Running { until, .. } | Self::WaitingForOutput { until } => Some(until),
            _ => None,
        }
    }

    pub(super) fn exited(&mut self, run: u64, exit: RepairExit, now: u64) {
        if matches!(*self, Self::Asking { run: id, .. } | Self::Running { run: id, .. } if id == run) {
            *self = match exit {
                RepairExit::Success => Self::WaitingForOutput { until: now + 5_000 },
                RepairExit::Cancelled => Self::NotApproved,
                RepairExit::Failed => Self::Failed,
            };
        }
    }

    pub(super) fn observe(&mut self, w: &World, now: u64) {
        if matches!(*self, Self::Running { until, .. } | Self::WaitingForOutput { until } if now >= until) {
            *self = Self::Failed;
        }
        *self = match *self {
            Self::WaitingForOutput { .. } | Self::Failed | Self::TurningOn { .. } | Self::TurnOnFailed if w.our_device => Self::Worked,
            Self::Failed if w.output_turned_off => Self::Idle,
            Self::TurningOn { until } if now >= until => Self::TurnOnFailed,
            Self::TurnOnFailed if !w.output_turned_off => Self::Idle,
            Self::Worked if !w.our_device => Self::Idle,
            Self::NotApproved if w.our_device || w.output_turned_off => Self::Idle,
            _ => return,
        };
    }

    pub(super) fn switched(&mut self) {
        if matches!(self, Self::Worked | Self::Failed) { *self = Self::Idle; }
    }

    pub(super) fn note(&self, w: &World) -> RepairNow {
        match self {
            Self::Running { .. } | Self::WaitingForOutput { .. } => RepairNow::Running,
            Self::Worked | Self::Failed | Self::TurningOn { .. } | Self::TurnOnFailed if w.our_device => RepairNow::Worked,
            Self::TurnOnFailed if w.output_turned_off => RepairNow::TurnOnFailed,
            _ if w.output_turned_off => RepairNow::TurnedOff,
            _ if !w.can_repair && !w.our_device => RepairNow::Unavailable,
            Self::Failed => RepairNow::DidntWork,
            Self::NotApproved => RepairNow::NotApproved,
            _ => RepairNow::NotAsked,
        }
    }
}
