use super::*;

impl ChannelSet {
    /// Creates an empty channel set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `channel` under `name`, returning the set for chaining.
    pub fn with_channel(
        mut self,
        name: impl Into<String>,
        channel: impl Channel + 'static,
    ) -> Self {
        self.add_channel(name, channel);
        self
    }

    /// Registers `channel` under `name`.
    pub fn add_channel(&mut self, name: impl Into<String>, channel: impl Channel + 'static) {
        self.channels.insert(name.into(), Box::new(channel));
    }

    /// Returns the current value of `name`, if any has been written.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.values.get(name)
    }

    /// Whether `name` is a registered channel.
    pub fn contains(&self, name: &str) -> bool {
        self.channels.contains_key(name)
    }

    /// Whether the channel `name` permits concurrent same-step writes. Errors
    /// if `name` is not a registered channel.
    pub fn allows_concurrent(&self, name: &str) -> Result<bool> {
        self.channel(name).map(|c| c.allows_concurrent())
    }

    /// Whether the barrier (or other) channel `name` has received everything it
    /// is waiting for. Non-barrier channels are always ready. Errors if `name`
    /// is not registered.
    pub fn is_ready(&self, name: &str) -> Result<bool> {
        let channel = self.channel(name)?;
        Ok(channel.is_ready(self.values.get(name)))
    }

    /// Folds `value` into the channel `name` via its merge rule. Errors with
    /// [`GraphError::Graph`] if `name` is not a registered channel.
    ///
    /// The unknown-channel check runs *before* any state is touched. If a
    /// registered channel's [`Channel::merge`] rejects the write (e.g. a
    /// [`Delta`] receiving a non-numeric value), the channel's prior value is
    /// dropped — a rejected write leaves the channel unset. This matches the
    /// executor's reducer contract, where a merge error discards the whole
    /// [`ChannelState`] for that step regardless.
    pub fn apply_update(&mut self, name: &str, value: Value) -> Result<()> {
        // Field-level borrows (channels immutable, values mutable) so the
        // current value can be *moved* into `merge` — accumulating channels then
        // fold in place rather than cloning the whole accumulated value.
        let channel = self
            .channels
            .get(name)
            .map(AsRef::as_ref)
            .ok_or_else(|| GraphError::Graph(format!("unknown channel `{name}`")))?;
        let current = self.values.remove(name);
        let merged = channel.merge(current, value)?;
        self.values.insert(name.to_string(), merged);
        Ok(())
    }

    /// Returns the tracked channel values as an ordered map, excluding
    /// [`Untracked`] channels. This is the durable/inspectable state view.
    pub fn snapshot(&self) -> BTreeMap<String, Value> {
        self.values
            .iter()
            .filter(|(name, _)| {
                self.channels
                    .get(*name)
                    .map(|c| c.is_tracked())
                    .unwrap_or(true)
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }

    /// Clears the value of every [`Ephemeral`] channel. Called at the start of a
    /// new step by [`ChannelState`].
    pub(crate) fn clear_ephemeral(&mut self) {
        let ephemeral: Vec<String> = self
            .channels
            .iter()
            .filter(|(_, c)| c.is_ephemeral())
            .map(|(name, _)| name.clone())
            .collect();
        for name in ephemeral {
            self.values.remove(&name);
        }
    }

    fn channel(&self, name: &str) -> Result<&dyn Channel> {
        self.channels
            .get(name)
            .map(AsRef::as_ref)
            .ok_or_else(|| GraphError::Graph(format!("unknown channel `{name}`")))
    }
}

// --- ChannelUpdate ---

impl ChannelUpdate {
    /// Creates an empty update.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a `(name, value)` write, returning the update for chaining.
    pub fn set(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.writes.push((name.into(), value.into()));
        self
    }

    /// Stamps the update with the producing node's superstep (`ctx.step`),
    /// enabling same-step concurrent-write conflict detection and ephemeral
    /// clearing. Without a stamp each update is treated as its own step.
    pub fn at_step(mut self, step: usize) -> Self {
        self.step = Some(step);
        self
    }

    /// Whether the update carries no writes.
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }
}

// --- ChannelState ---

impl ChannelState {
    /// Creates a state with no channels.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `channel` under `name`, returning the state for chaining. Use
    /// this to declare a graph's channel schema before running.
    pub fn with_channel(
        mut self,
        name: impl Into<String>,
        channel: impl Channel + 'static,
    ) -> Self {
        self.set.add_channel(name, channel);
        self
    }

    /// Borrows the underlying [`ChannelSet`].
    pub fn channels(&self) -> &ChannelSet {
        &self.set
    }

    /// Returns the current value of channel `name`, if written.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.set.get(name)
    }

    /// Returns the tracked channel values (see [`ChannelSet::snapshot`]).
    pub fn snapshot(&self) -> BTreeMap<String, Value> {
        self.set.snapshot()
    }

    /// Whether channel `name` is a satisfied barrier (see
    /// [`ChannelSet::is_ready`]).
    pub fn is_ready(&self, name: &str) -> Result<bool> {
        self.set.is_ready(name)
    }

    /// Folds a [`ChannelUpdate`] into this state, dispatching each write to its
    /// channel's merge rule. This is the core reducer step.
    ///
    /// When the update is stamped (via [`ChannelUpdate::at_step`]) with a step
    /// number that differs from the last one seen, the per-step write tracking
    /// is reset and [`Ephemeral`] channels are cleared before the writes apply.
    /// A second write to a non-aggregate channel within the same stamped step
    /// raises [`GraphError::InvalidConcurrentUpdate`].
    pub fn merge(mut self, update: ChannelUpdate) -> Result<Self> {
        match update.step {
            Some(step) if step != self.current_step => {
                self.current_step = step;
                self.step_writes.clear();
                self.set.clear_ephemeral();
            }
            Some(_) => {}
            None => {
                // Unstamped updates are independent: no cross-update detection.
                self.step_writes.clear();
            }
        }

        // Distinct channels touched by this single update (a node writing the
        // same channel twice in one update is last-wins, not a conflict).
        let mut distinct: Vec<&str> = Vec::new();
        for (name, _) in &update.writes {
            if !distinct.contains(&name.as_str()) {
                distinct.push(name.as_str());
            }
        }

        // Validate before mutating so a conflicting step never commits partial
        // writes.
        for name in &distinct {
            let allows = self.set.allows_concurrent(name)?;
            let count = self.step_writes.get(*name).copied().unwrap_or(0) + 1;
            if count > 1 && !allows {
                return Err(GraphError::InvalidConcurrentUpdate(format!(
                    "channel `{name}` received {count} concurrent writes in one step but is not an aggregate channel"
                )));
            }
        }

        let touched: HashSet<String> = distinct.iter().map(|n| n.to_string()).collect();
        for name in touched {
            *self.step_writes.entry(name).or_insert(0) += 1;
        }
        for (name, value) in update.writes {
            self.set.apply_update(&name, value)?;
        }
        Ok(self)
    }
}

/// `ChannelState` is its own [`StateReducer`]: the `&self` receiver is unused
/// (merge rules live in the running `state`'s [`ChannelSet`]), so any
/// `ChannelState` may be passed to `set_reducer`.
impl StateReducer<ChannelState, ChannelUpdate> for ChannelState {
    fn apply(&self, state: ChannelState, update: ChannelUpdate) -> Result<ChannelState> {
        state.merge(update)
    }
}
