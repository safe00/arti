//! Code for building paths to an exit relay.

use super::TorPath;
use crate::{DirInfo, Error, Result, TargetPort};
use rand::Rng;
use tor_netdir::{NetDir, Relay, WeightRole};

/// Internal representation of PathBuilder.
enum ExitPathBuilderInner<'a> {
    /// Request a path that allows exit to the given TargetPort's.
    WantsPorts(Vec<TargetPort>),

    /// Request a path that uses a given relay as exit node.
    ChosenExit(Relay<'a>),
}

/// A PathBuilder that builds a path to an exit relay supporting a given
/// set of ports.
pub struct ExitPathBuilder<'a> {
    /// The inner ExitPathBuilder state.
    inner: ExitPathBuilderInner<'a>,
}

impl<'a> ExitPathBuilder<'a> {
    /// Create a new builder that will try to get an exit relay
    /// containing all the ports in `ports`.
    pub fn from_target_ports(wantports: impl IntoIterator<Item = TargetPort>) -> Self {
        Self {
            inner: ExitPathBuilderInner::WantsPorts(wantports.into_iter().collect()),
        }
    }

    /// Create a new builder that will try to build a path with the given exit
    /// relay as the last hop.
    pub fn from_chosen_exit(exit_relay: Relay<'a>) -> Self {
        Self {
            inner: ExitPathBuilderInner::ChosenExit(exit_relay),
        }
    }

    /// Find a suitable exit node from either the chosen exit or from the network directory.
    fn pick_exit<R: Rng>(&self, rng: &mut R, netdir: &'a NetDir) -> Result<Relay<'a>> {
        match &self.inner {
            ExitPathBuilderInner::WantsPorts(wantports) => Ok(netdir
                .pick_relay(rng, WeightRole::Exit, |r| {
                    wantports.iter().all(|p| p.is_supported_by(r))
                })
                .ok_or_else(|| Error::NoRelays("No exit relay found".into()))?),

            ExitPathBuilderInner::ChosenExit(exit_relay) => Ok(exit_relay.clone()),
        }
    }

    /// Try to create and return a path corresponding to the requirements of
    /// this builder.
    pub fn pick_path<R: Rng>(&self, rng: &mut R, netdir: DirInfo<'a>) -> Result<TorPath<'a>> {
        // TODO: implement guards
        let netdir = match netdir {
            DirInfo::Fallbacks(_) => return Err(Error::NeedConsensus),
            DirInfo::Directory(d) => d,
        };
        let exit = self.pick_exit(rng, netdir)?;

        let middle = netdir
            .pick_relay(rng, WeightRole::Middle, |r| !r.in_same_family(&exit))
            .ok_or_else(|| Error::NoRelays("No middle relay found".into()))?;

        let entry = netdir
            .pick_relay(rng, WeightRole::Guard, |r| {
                !r.in_same_family(&middle) && !r.in_same_family(&exit)
            })
            .ok_or_else(|| Error::NoRelays("No entry relay found".into()))?;

        Ok(TorPath::new_multihop(vec![entry, middle, exit]))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::path::{assert_same_path_when_owned, OwnedPath, TorPathInner};
    use std::convert::TryInto;
    use tor_linkspec::ChanTarget;
    use tor_netdir::testnet;

    fn assert_exit_path_ok<'a>(relays: &[Relay<'a>]) {
        assert_eq!(relays.len(), 3);

        // TODO: Eventually assert that r1 has Guard, once we enforce that.

        let r1 = &relays[0];
        let r2 = &relays[1];
        let r3 = &relays[2];

        assert!(r1.ed_identity() != r2.ed_identity());
        assert!(r1.ed_identity() != r3.ed_identity());
        assert!(r2.ed_identity() != r3.ed_identity());

        assert!(!r1.in_same_family(r2));
        assert!(!r1.in_same_family(r3));
        assert!(!r2.in_same_family(r3));
    }

    #[test]
    fn by_ports() {
        let mut rng = rand::thread_rng();
        let netdir = testnet::construct_netdir();
        let ports = vec![TargetPort::ipv4(443), TargetPort::ipv4(1119)];
        let dirinfo = (&netdir).into();

        for _ in 0..1000 {
            let path = ExitPathBuilder::from_target_ports(ports.clone())
                .pick_path(&mut rng, dirinfo)
                .unwrap();

            assert_same_path_when_owned(&path);

            if let TorPathInner::Path(p) = path.inner {
                assert_exit_path_ok(&p[..]);
                let exit = &p[2];
                assert!(exit.ipv4_policy().allows_port(1119));
            } else {
                panic!("Generated the wrong kind of path");
            }
        }

        let chosen = netdir.by_id(&[0x20; 32].into()).unwrap();

        for _ in 0..1000 {
            let path = ExitPathBuilder::from_chosen_exit(chosen.clone())
                .pick_path(&mut rng, dirinfo)
                .unwrap();
            assert_same_path_when_owned(&path);
            if let TorPathInner::Path(p) = path.inner {
                assert_exit_path_ok(&p[..]);
                let exit = &p[2];
                assert_eq!(exit.ed_identity(), chosen.ed_identity());
            } else {
                panic!("Generated the wrong kind of path");
            }
        }
    }

    #[test]
    fn empty_path() {
        // This shouldn't actually be constructable IRL, but let's test to
        // make sure our code can handle it.
        let bogus_path = TorPath {
            inner: TorPathInner::Path(vec![]),
        };

        assert!(bogus_path.exit_relay().is_none());
        assert!(bogus_path.exit_policy().is_none());
        assert_eq!(bogus_path.len(), 0);

        let owned: Result<OwnedPath> = (&bogus_path).try_into();
        assert!(owned.is_err());

        // This should also be unconstructable.
        let owned_bogus = OwnedPath::Normal(vec![]);
        assert!(owned_bogus.first_hop().is_err());
    }
}
