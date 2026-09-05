//! CognitiveChain: a Layer-1 blockchain secured by verifiable tensor computation.
//!
//! The crate is split into a consensus core and the services around it:
//!
//! * [`types`]  - wire formats and hash pre-images (consensus critical)
//! * [`pouw`]   - the Proof-of-Useful-Work task, commitment and challenge scheme
//! * [`state`]  - the replicated state machine and emission policy
//! * [`chain`]  - validation, chain selection and block production
//! * [`store`]  - durable block storage
//! * [`rpc`]    - the JSON-RPC surface used by miners and wallets
//! * [`p2p`]    - node-to-node gossip
//! * [`pool`]   - mining pool with verified shares and PPLNS payouts
//! * [`wallet_ui`] - local graphical wallet, signing in-process

pub mod chain;
pub mod client;
pub mod crypto;
pub mod genesis;
pub mod mempool;
pub mod p2p;
pub mod pool;
pub mod pouw;
pub mod rpc;
pub mod state;
pub mod store;
pub mod types;
pub mod wallet_ui;
