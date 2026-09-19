//! Everything rox sends over the wire, and the service identities a build
//! sends it as. The enrichment providers, the signed Last.fm calls and the
//! Libre.fm host they also serve, the ListenBrainz submissions, and the
//! keys baked in at compile time are all defined here. Nothing here draws anything
//! and every call blocks, so the app runs them on its background executor.

pub mod discord;
pub mod lastfm;
pub mod librefm;
pub mod listenbrainz;
pub mod providers;
pub mod sources;
