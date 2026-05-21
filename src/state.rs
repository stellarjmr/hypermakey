#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HyperAction {
    None,
    Press,
    Release,
}

#[derive(Debug, Default)]
pub struct HyperState {
    caps_down: bool,
    hyper_active: bool,
}

impl HyperState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle_caps_value(&mut self, pressed: bool) -> HyperAction {
        if pressed {
            self.handle_caps_down()
        } else {
            self.handle_caps_up()
        }
    }

    pub fn reset(&mut self) -> HyperAction {
        self.caps_down = false;

        if self.hyper_active {
            self.hyper_active = false;
            HyperAction::Release
        } else {
            HyperAction::None
        }
    }

    fn handle_caps_down(&mut self) -> HyperAction {
        if self.caps_down {
            return HyperAction::None;
        }

        self.caps_down = true;

        if self.hyper_active {
            HyperAction::None
        } else {
            self.hyper_active = true;
            HyperAction::Press
        }
    }

    fn handle_caps_up(&mut self) -> HyperAction {
        self.caps_down = false;

        if self.hyper_active {
            self.hyper_active = false;
            HyperAction::Release
        } else {
            HyperAction::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{HyperAction, HyperState};

    #[test]
    fn first_caps_down_presses_hyper() {
        let mut state = HyperState::new();

        assert_eq!(state.handle_caps_value(true), HyperAction::Press);
    }

    #[test]
    fn repeated_caps_down_is_ignored() {
        let mut state = HyperState::new();

        assert_eq!(state.handle_caps_value(true), HyperAction::Press);
        assert_eq!(state.handle_caps_value(true), HyperAction::None);
    }

    #[test]
    fn caps_up_releases_active_hyper() {
        let mut state = HyperState::new();

        assert_eq!(state.handle_caps_value(true), HyperAction::Press);
        assert_eq!(state.handle_caps_value(false), HyperAction::Release);
    }

    #[test]
    fn repeated_caps_up_is_ignored() {
        let mut state = HyperState::new();

        assert_eq!(state.handle_caps_value(false), HyperAction::None);
        assert_eq!(state.handle_caps_value(false), HyperAction::None);
    }

    #[test]
    fn reset_releases_active_hyper_once() {
        let mut state = HyperState::new();

        assert_eq!(state.handle_caps_value(true), HyperAction::Press);
        assert_eq!(state.reset(), HyperAction::Release);
        assert_eq!(state.reset(), HyperAction::None);
    }
}
