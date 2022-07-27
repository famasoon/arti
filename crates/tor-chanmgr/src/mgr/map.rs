//! Simple implementation for the internal map state of a ChanMgr.

use std::time::Duration;

use super::{AbstractChannel, Pending};
use crate::{ChannelConfig, Dormancy, Error, Result};

use std::collections::{hash_map, HashMap};
use std::result::Result as StdResult;
use std::sync::Arc;
use tor_config::PaddingLevel;
use tor_error::{internal, into_internal};
use tor_netdir::{params::CHANNEL_PADDING_TIMEOUT_UPPER_BOUND, NetDir};
use tor_proto::channel::padding::Parameters as PaddingParameters;
use tor_proto::channel::padding::ParametersBuilder as PaddingParametersBuilder;
use tor_proto::ChannelsParams;
use tor_units::{BoundedInt32, IntegerMilliseconds};
use tracing::info;

/// A map from channel id to channel state, plus necessary auxiliary state
///
/// We make this a separate type instead of just using
/// `Mutex<HashMap<...>>` to limit the amount of code that can see and
/// lock the Mutex here.  (We're using a blocking mutex close to async
/// code, so we need to be careful.)
pub(crate) struct ChannelMap<C: AbstractChannel> {
    /// The data, within a lock
    inner: std::sync::Mutex<Inner<C>>,
}

/// A map from channel id to channel state, plus necessary auxiliary state - inside lock
struct Inner<C: AbstractChannel> {
    /// A map from identity to channel, or to pending channel status.
    ///
    /// (Danger: this uses a blocking mutex close to async code.  This mutex
    /// must never be held while an await is happening.)
    channels: HashMap<C::Ident, ChannelState<C>>,

    /// Parameters for channels that we create, and that all existing channels are using
    ///
    /// Will be updated by a background task, which also notifies all existing
    /// `Open` channels via `channels`.
    ///
    /// (Must be protected by the same lock as `channels`, or a channel might be
    /// created using being-replaced parameters, but not get an update.)
    channels_params: ChannelsParams,

    /// The configuration (from the config file or API caller)
    config: ChannelConfig,

    /// Dormancy
    dormancy: Dormancy,
}

/// Structure that can only be constructed from within this module.
/// Used to make sure that only we can construct ChannelState::Poisoned.
pub(crate) struct Priv {
    /// (This field is private)
    _unused: (),
}

/// The state of a channel (or channel build attempt) within a map.
pub(crate) enum ChannelState<C> {
    /// An open channel.
    ///
    /// This channel might not be usable: it might be closing or
    /// broken.  We need to check its is_usable() method before
    /// yielding it to the user.
    Open(OpenEntry<C>),
    /// A channel that's getting built.
    Building(Pending<C>),
    /// A temporary invalid state.
    ///
    /// We insert this into the map temporarily as a placeholder in
    /// `change_state()`.
    Poisoned(Priv),
}

/// An open channel entry.
#[derive(Clone)]
pub(crate) struct OpenEntry<C> {
    /// The underlying open channel.
    pub(crate) channel: C,
    /// The maximum unused duration allowed for this channel.
    pub(crate) max_unused_duration: Duration,
}

impl<C: Clone> ChannelState<C> {
    /// Create a new shallow copy of this ChannelState.
    #[cfg(test)]
    fn clone_ref(&self) -> Result<Self> {
        use ChannelState::*;
        match self {
            Open(ent) => Ok(Open(ent.clone())),
            Building(pending) => Ok(Building(pending.clone())),
            Poisoned(_) => Err(Error::Internal(internal!("Poisoned state in channel map"))),
        }
    }

    /// For testing: either give the Open channel inside this state,
    /// or panic if there is none.
    #[cfg(test)]
    fn unwrap_open(&mut self) -> &mut C {
        match self {
            ChannelState::Open(ent) => &mut ent.channel,
            _ => panic!("Not an open channel"),
        }
    }
}

/// Type of the `nf_ito_*` netdir parameters, convenience alias
type NfIto = IntegerMilliseconds<BoundedInt32<0, CHANNEL_PADDING_TIMEOUT_UPPER_BOUND>>;

/// Extract from a `NetDir` which we need, conveniently organised for our processing
///
/// This type serves two functions at once:
///
///  1. Being a subset of the parameters from the netdir, we can copy it out of
///     the netdir, before we do more complex processing - and, in particular,
///     before we obtain the lock on `inner` (which we need to actually handle the update,
///     because we need to combine information from the config with that from the netdir).
///
///  2. Rather than four separate named fields, it has arrays, so that it is easy to
///     select the values without error-prone recapitulation of field names.
#[derive(Debug, Clone)]
struct NetDirExtract {
    /// `nf_ito_*`
    ///
    /// `nf_ito[ 0=normal, 1=reduced ][ 0=low, 1=high ]`
    // TODO we could use some enum or IndexVec or something to make this less `0` and `1`
    nf_ito: [[NfIto; 2]; 2],
}

impl From<&NetDir> for NetDirExtract {
    fn from(netdir: &NetDir) -> Self {
        let p = &netdir.params();
        NetDirExtract {
            nf_ito: [
                [p.nf_ito_low, p.nf_ito_high],
                [p.nf_ito_low_reduced, p.nf_ito_high_reduced],
            ],
        }
    }
}

impl<C: AbstractChannel> ChannelState<C> {
    /// Return an error if `ident`is definitely not a matching
    /// matching identity for this state.
    fn check_ident(&self, ident: &C::Ident) -> Result<()> {
        match self {
            ChannelState::Open(ent) => {
                if ent.channel.ident() == ident {
                    Ok(())
                } else {
                    Err(Error::Internal(internal!("Identity mismatch")))
                }
            }
            ChannelState::Poisoned(_) => {
                Err(Error::Internal(internal!("Poisoned state in channel map")))
            }
            ChannelState::Building(_) => Ok(()),
        }
    }

    /// Return true if a channel is ready to expire.
    /// Update `expire_after` if a smaller duration than
    /// the given value is required to expire this channel.
    fn ready_to_expire(&self, expire_after: &mut Duration) -> bool {
        if let ChannelState::Open(ent) = self {
            let unused_duration = ent.channel.duration_unused();
            if let Some(unused_duration) = unused_duration {
                let max_unused_duration = ent.max_unused_duration;

                if let Some(remaining) = max_unused_duration.checked_sub(unused_duration) {
                    *expire_after = std::cmp::min(*expire_after, remaining);
                    false
                } else {
                    true
                }
            } else {
                // still in use
                false
            }
        } else {
            false
        }
    }
}

impl<C: AbstractChannel> ChannelMap<C> {
    /// Create a new empty ChannelMap.
    pub(crate) fn new(config: ChannelConfig, dormancy: Dormancy) -> Self {
        let channels_params = ChannelsParams::default();
        ChannelMap {
            inner: std::sync::Mutex::new(Inner {
                channels: HashMap::new(),
                config,
                channels_params,
                dormancy,
            }),
        }
    }

    /// Return the channel state for the given identity, if any.
    #[cfg(test)]
    pub(crate) fn get(&self, ident: &C::Ident) -> Result<Option<ChannelState<C>>> {
        let inner = self.inner.lock()?;
        inner
            .channels
            .get(ident)
            .map(ChannelState::clone_ref)
            .transpose()
    }

    /// Replace the channel state for `ident` with `newval`, and return the
    /// previous value if any.
    #[cfg(test)]
    pub(crate) fn replace(
        &self,
        ident: C::Ident,
        newval: ChannelState<C>,
    ) -> Result<Option<ChannelState<C>>> {
        newval.check_ident(&ident)?;
        let mut inner = self.inner.lock()?;
        Ok(inner.channels.insert(ident, newval))
    }

    /// Replace the channel state for `ident` with the return value from `func`,
    /// and return the previous value if any.
    ///
    /// Passes a snapshot of the current global channels parameters to `func`.
    /// If those parameters are copied by `func` into an [`AbstractChannel`]
    /// `func` must ensure that that `AbstractChannel` is returned,
    /// so that it will be properly registered and receive params updates.
    pub(crate) fn replace_with_params<F>(
        &self,
        ident: C::Ident,
        func: F,
    ) -> Result<Option<ChannelState<C>>>
    where
        F: FnOnce(&ChannelsParams) -> Result<ChannelState<C>>,
    {
        let mut inner = self.inner.lock()?;
        let newval = func(&inner.channels_params)?;
        newval.check_ident(&ident)?;
        Ok(inner.channels.insert(ident, newval))
    }

    /// Remove and return the state for `ident`, if any.
    pub(crate) fn remove(&self, ident: &C::Ident) -> Result<Option<ChannelState<C>>> {
        let mut inner = self.inner.lock()?;
        Ok(inner.channels.remove(ident))
    }

    /// Remove every unusable state from the map.
    #[cfg(test)]
    pub(crate) fn remove_unusable(&self) -> Result<()> {
        let mut inner = self.inner.lock()?;
        inner.channels.retain(|_, state| match state {
            ChannelState::Poisoned(_) => false,
            ChannelState::Open(ent) => ent.channel.is_usable(),
            ChannelState::Building(_) => true,
        });
        Ok(())
    }

    /// Replace the state whose identity is `ident` with a new state.
    ///
    /// The provided function `func` is invoked on the old state (if
    /// any), and must return a tuple containing an optional new
    /// state, and an arbitrary return value for this function.
    ///
    /// Because `func` is run while holding the lock on this object,
    /// it should be fast and nonblocking.  In return, you can be sure
    /// that it's running atomically with respect to other accessors
    /// of this map.
    ///
    /// If `func` panics, or if it returns a channel with a different
    /// identity, this position in the map will be become unusable and
    /// future accesses to that position may fail.
    pub(crate) fn change_state<F, V>(&self, ident: &C::Ident, func: F) -> Result<V>
    where
        F: FnOnce(Option<ChannelState<C>>) -> (Option<ChannelState<C>>, V),
    {
        use hash_map::Entry::*;
        let mut inner = self.inner.lock()?;
        let entry = inner.channels.entry(ident.clone());
        match entry {
            Occupied(mut occupied) => {
                // Temporarily replace the entry for this identity with
                // a poisoned entry.
                let mut oldent = ChannelState::Poisoned(Priv { _unused: () });
                std::mem::swap(occupied.get_mut(), &mut oldent);
                let (newval, output) = func(Some(oldent));
                match newval {
                    Some(mut newent) => {
                        newent.check_ident(ident)?;
                        std::mem::swap(occupied.get_mut(), &mut newent);
                    }
                    None => {
                        occupied.remove();
                    }
                };
                Ok(output)
            }
            Vacant(vacant) => {
                let (newval, output) = func(None);
                if let Some(newent) = newval {
                    newent.check_ident(ident)?;
                    vacant.insert(newent);
                }
                Ok(output)
            }
        }
    }

    /// Reconfigure all channels as necessary
    ///
    /// (By reparameterising channels as needed)
    /// This function will handle
    ///   - netdir update
    ///   - a reconfiguration
    ///   - dormancy (TODO, this doesn't do anything yet)
    ///
    /// For `new_config` and `new_dormancy`, `None` means "no change to previous info".
    pub(super) fn reconfigure_general(
        &self,
        new_config: Option<&ChannelConfig>,
        new_dormancy: Option<Dormancy>,
        netdir: tor_netdir::Result<Arc<NetDir>>,
    ) -> StdResult<(), tor_error::Bug> {
        use ChannelState as CS;

        // TODO support dormant mode
        // TODO when entering/leaving dormant mode, send CELL_PADDING_NEGOTIATE to peers
        // TODO with reduced padding, send CELL_PADDING_NEGOTIATE

        // TODO when we support operation as a relay, inter-relay channels ought
        // not to get padding.
        let netdir = {
            let extract = netdir
                .as_ref()
                .map(|n| NetDirExtract::from(&**n))
                .map_err(|_| ());
            // Drop the `Arc<NetDir>` as soon as we have got what we need from it,
            // before we take the channel map lock.
            drop(netdir);
            extract
        };

        let mut inner = self
            .inner
            .lock()
            .map_err(|_| internal!("poisonned channel manager"))?;

        if let Some(new_config) = new_config {
            inner.config = new_config.clone();
        }
        if let Some(new_dormancy) = new_dormancy {
            inner.dormancy = new_dormancy;
        }

        let padding_parameters = padding_parameters(inner.config.padding, netdir.as_ref())?;
        // TODO if this is equal to all_zeroes(), do not enable padding
        // (when we enable padding at all, which we do not do yet...)

        let update = inner
            .channels_params
            .start_update()
            .padding_parameters(padding_parameters)
            .finish();
        let update = if let Some(u) = update {
            u
        } else {
            return Ok(());
        };
        let update = Arc::new(update);

        for channel in inner.channels.values_mut() {
            let channel = match channel {
                CS::Open(OpenEntry { channel, .. }) => channel,
                CS::Building(_) | CS::Poisoned(_) => continue,
            };
            // Ignore error (which simply means the channel is closed or gone)
            let _ = channel.reparameterize(update.clone());
        }
        Ok(())
    }

    /// Expire all channels that have been unused for too long.
    ///
    /// Return a Duration until the next time at which
    /// a channel _could_ expire.
    pub(crate) fn expire_channels(&self) -> Duration {
        let mut ret = Duration::from_secs(180);
        self.inner
            .lock()
            .expect("Poisoned lock")
            .channels
            .retain(|_id, chan| !chan.ready_to_expire(&mut ret));
        ret
    }
}

/// Given a `NetDirExtract` and whether we're reducing padding, return a `PaddingParameters`
///
/// With `PaddingLevel::None`, will return `PaddingParameters::all_zeroes`; but
/// does not account for padding being enabled/disabled other ways than via the config.
fn padding_parameters(
    config: PaddingLevel,
    netdir: StdResult<&NetDirExtract, &()>,
) -> StdResult<PaddingParameters, tor_error::Bug> {
    let reduced = match config {
        PaddingLevel::Reduced => true,
        PaddingLevel::Normal => false,
        PaddingLevel::None => return Ok(PaddingParameters::all_zeroes()),
    };

    Ok(match netdir {
        Ok(netdir) => {
            let mut p = PaddingParametersBuilder::default();
            let () = (|| {
                let nf_ito = netdir.nf_ito[usize::from(reduced)];
                let get_timing_param =
                    |index: usize| nf_ito[index].try_map(|bounded| bounded.get().try_into());
                let low = get_timing_param(0).map_err(|_| "low value arithmetic overflow?!")?;
                let high = get_timing_param(1).map_err(|_| "high value arithmetic overflow?!")?;
                if low > high {
                    return Err("low > high");
                }
                p.low_ms(low);
                p.high_ms(high);
                Ok::<_, &'static str>(())
            })()
            .unwrap_or_else(|e| {
                info!(
                    "consensus channel padding parameters wrong, using defaults: {}",
                    &e
                );
            });

            p.build()
                .map_err(into_internal!("failed to build padding parameters"))?
        }
        Err(()) => {
            // TODO we should use a fallback here so that config overrides take effect,
            // as discussed in https://gitlab.torproject.org/tpo/core/arti/-/issues/528
            if reduced {
                PaddingParameters::default_reduced()
            } else {
                PaddingParameters::default()
            }
        }
    })
}

#[cfg(test)]
mod test {
    // @@ begin test lint list maintained by maint/add_warning @@
    #![allow(clippy::bool_assert_comparison)]
    #![allow(clippy::clone_on_copy)]
    #![allow(clippy::dbg_macro)]
    #![allow(clippy::print_stderr)]
    #![allow(clippy::print_stdout)]
    #![allow(clippy::unwrap_used)]
    //! <!-- @@ end test lint list maintained by maint/add_warning @@ -->

    use super::*;
    use std::sync::Arc;
    use tor_proto::channel::params::ChannelsParamsUpdates;
    use tor_proto::channel::ChannelUsage;

    fn new_test_channel_map<C: AbstractChannel>() -> ChannelMap<C> {
        ChannelMap::new(ChannelConfig::default(), Default::default())
    }

    #[derive(Eq, PartialEq, Clone, Debug)]
    struct FakeChannel {
        ident: &'static str,
        usable: bool,
        unused_duration: Option<u64>,
        params_update: Option<Arc<ChannelsParamsUpdates>>,
    }
    impl AbstractChannel for FakeChannel {
        type Ident = u8;
        fn ident(&self) -> &Self::Ident {
            &self.ident.as_bytes()[0]
        }
        fn is_usable(&self) -> bool {
            self.usable
        }
        fn duration_unused(&self) -> Option<Duration> {
            self.unused_duration.map(Duration::from_secs)
        }
        fn reparameterize(&mut self, update: Arc<ChannelsParamsUpdates>) -> tor_proto::Result<()> {
            self.params_update = Some(update);
            Ok(())
        }
        fn note_usage(&self, _usage: ChannelUsage) -> StdResult<(), tor_error::Bug> {
            Ok(())
        }
    }
    fn ch(ident: &'static str) -> ChannelState<FakeChannel> {
        let channel = FakeChannel {
            ident,
            usable: true,
            unused_duration: None,
            params_update: None,
        };
        ChannelState::Open(OpenEntry {
            channel,
            max_unused_duration: Duration::from_secs(180),
        })
    }
    fn ch_with_details(
        ident: &'static str,
        max_unused_duration: Duration,
        unused_duration: Option<u64>,
    ) -> ChannelState<FakeChannel> {
        let channel = FakeChannel {
            ident,
            usable: true,
            unused_duration,
            params_update: None,
        };
        ChannelState::Open(OpenEntry {
            channel,
            max_unused_duration,
        })
    }
    fn closed(ident: &'static str) -> ChannelState<FakeChannel> {
        let channel = FakeChannel {
            ident,
            usable: false,
            unused_duration: None,
            params_update: None,
        };
        ChannelState::Open(OpenEntry {
            channel,
            max_unused_duration: Duration::from_secs(180),
        })
    }

    #[test]
    fn simple_ops() {
        let map = new_test_channel_map();
        use ChannelState::Open;

        assert!(map.replace(b'h', ch("hello")).unwrap().is_none());
        assert!(map.replace(b'w', ch("wello")).unwrap().is_none());

        match map.get(&b'h') {
            Ok(Some(Open(ent))) if ent.channel.ident == "hello" => {}
            _ => panic!(),
        }

        assert!(map.get(&b'W').unwrap().is_none());

        match map.replace(b'h', ch("hebbo")) {
            Ok(Some(Open(ent))) if ent.channel.ident == "hello" => {}
            _ => panic!(),
        }

        assert!(map.remove(&b'Z').unwrap().is_none());
        match map.remove(&b'h') {
            Ok(Some(Open(ent))) if ent.channel.ident == "hebbo" => {}
            _ => panic!(),
        }
    }

    #[test]
    fn rmv_unusable() {
        let map = new_test_channel_map();

        map.replace(b'm', closed("machen")).unwrap();
        map.replace(b'f', ch("feinen")).unwrap();
        map.replace(b'w', closed("wir")).unwrap();
        map.replace(b'F', ch("Fug")).unwrap();

        map.remove_unusable().unwrap();

        assert!(map.get(&b'm').unwrap().is_none());
        assert!(map.get(&b'w').unwrap().is_none());
        assert!(map.get(&b'f').unwrap().is_some());
        assert!(map.get(&b'F').unwrap().is_some());
    }

    #[test]
    fn change() {
        let map = new_test_channel_map();

        map.replace(b'w', ch("wir")).unwrap();
        map.replace(b'm', ch("machen")).unwrap();
        map.replace(b'f', ch("feinen")).unwrap();
        map.replace(b'F', ch("Fug")).unwrap();

        //  Replace Some with Some.
        let (old, v) = map
            .change_state(&b'F', |state| (Some(ch("FUG")), (state, 99_u8)))
            .unwrap();
        assert_eq!(old.unwrap().unwrap_open().ident, "Fug");
        assert_eq!(v, 99);
        assert_eq!(map.get(&b'F').unwrap().unwrap().unwrap_open().ident, "FUG");

        // Replace Some with None.
        let (old, v) = map
            .change_state(&b'f', |state| (None, (state, 123_u8)))
            .unwrap();
        assert_eq!(old.unwrap().unwrap_open().ident, "feinen");
        assert_eq!(v, 123);
        assert!(map.get(&b'f').unwrap().is_none());

        // Replace None with Some.
        let (old, v) = map
            .change_state(&b'G', |state| (Some(ch("Geheimnisse")), (state, "Hi")))
            .unwrap();
        assert!(old.is_none());
        assert_eq!(v, "Hi");
        assert_eq!(
            map.get(&b'G').unwrap().unwrap().unwrap_open().ident,
            "Geheimnisse"
        );

        // Replace None with None
        let (old, v) = map
            .change_state(&b'Q', |state| (None, (state, "---")))
            .unwrap();
        assert!(old.is_none());
        assert_eq!(v, "---");
        assert!(map.get(&b'Q').unwrap().is_none());

        // Try replacing None with invalid entry (with mismatched ID)
        let e = map.change_state(&b'P', |state| (Some(ch("Geheimnisse")), (state, "Hi")));
        assert!(matches!(e, Err(Error::Internal(_))));
        assert!(matches!(map.get(&b'P'), Ok(None)));

        // Try replacing Some with invalid entry (mismatched ID)
        let e = map.change_state(&b'G', |state| (Some(ch("Wobbledy")), (state, "Hi")));
        assert!(matches!(e, Err(Error::Internal(_))));
        assert!(matches!(map.get(&b'G'), Err(Error::Internal(_))));
    }

    #[test]
    fn reparameterise_via_netdir() {
        let map = new_test_channel_map();

        // Set some non-default parameters so that we can tell when an update happens
        let _ = map
            .inner
            .lock()
            .unwrap()
            .channels_params
            .start_update()
            .padding_parameters(
                PaddingParametersBuilder::default()
                    .low_ms(1234.into())
                    .build()
                    .unwrap(),
            )
            .finish();

        assert!(map.replace(b't', ch("track")).unwrap().is_none());

        let netdir = tor_netdir::testnet::construct_netdir()
            .unwrap_if_sufficient()
            .unwrap();
        let netdir = Arc::new(netdir);

        let with_ch = |f: &dyn Fn(&mut FakeChannel)| {
            let mut inner = map.inner.lock().unwrap();
            let ch = inner.channels.get_mut(&b't').unwrap().unwrap_open();
            f(ch);
        };

        eprintln!("-- process a default netdir, which should send an update --");
        map.reconfigure_general(None, None, Ok(netdir.clone()))
            .unwrap();
        with_ch(&|ch| {
            assert_eq!(
                format!("{:?}", ch.params_update.take().unwrap()),
                // evade field visibility by (ab)using Debug impl
                "ChannelsParamsUpdates { padding_enable: None, \
                    padding_parameters: Some(Parameters { \
                        low_ms: IntegerMilliseconds { value: 1500 }, \
                        high_ms: IntegerMilliseconds { value: 9500 } }) }"
            );
        });
        eprintln!();

        eprintln!("-- process a default netdir again, which should *not* send an update --");
        map.reconfigure_general(None, None, Ok(netdir)).unwrap();
        with_ch(&|ch| assert_eq!(ch.params_update, None));
    }

    #[test]
    fn expire_channels() {
        let map = new_test_channel_map();

        // Channel that has been unused beyond max duration allowed is expired
        map.replace(
            b'w',
            ch_with_details("wello", Duration::from_secs(180), Some(181)),
        )
        .unwrap();

        // Minimum value of max unused duration is 180 seconds
        assert_eq!(180, map.expire_channels().as_secs());
        assert!(map.get(&b'w').unwrap().is_none());

        let map = new_test_channel_map();

        // Channel that has been unused for shorter than max unused duration
        map.replace(
            b'w',
            ch_with_details("wello", Duration::from_secs(180), Some(120)),
        )
        .unwrap();

        map.replace(
            b'y',
            ch_with_details("yello", Duration::from_secs(180), Some(170)),
        )
        .unwrap();

        // Channel that has been unused beyond max duration allowed is expired
        map.replace(
            b'g',
            ch_with_details("gello", Duration::from_secs(180), Some(181)),
        )
        .unwrap();

        // Closed channel should be retained
        map.replace(b'h', closed("hello")).unwrap();

        // Return duration until next channel expires
        assert_eq!(10, map.expire_channels().as_secs());
        assert!(map.get(&b'w').unwrap().is_some());
        assert!(map.get(&b'y').unwrap().is_some());
        assert!(map.get(&b'h').unwrap().is_some());
        assert!(map.get(&b'g').unwrap().is_none());
    }
}
