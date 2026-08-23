// Copyright 2016 Mozilla Foundation
// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::api::{request_shutdown, request_stats};
use crate::cache::CacheMode;
use crate::cache::disk::DiskCache;
use crate::client::connect_to_server;
use crate::commands::do_compile;
use crate::config::PreprocessorCacheModeConfig;
use crate::jobserver::Client;
use crate::mock_command::*;
use crate::server::{DistClientContainer, SccacheServer, ServerMessage};
use crate::test::utils::*;
use fs::File;
use fs_err as fs;
use futures::channel::oneshot::{self, Sender};
use std::io::{Cursor, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;
use tokio::runtime::Runtime;

/// Options for running the server in tests.
#[derive(Default)]
struct ServerOptions {
    /// The server's idle shutdown timeout.
    idle_timeout: Option<u64>,
    /// The maximum size of the disk cache.
    cache_size: Option<u64>,
}

/// Run a server on a background thread, and return a tuple of useful things.
///
/// * The port on which the server is listening.
/// * A `Sender` which can be used to send messages to the server.
///   (Most usefully, ServerMessage::Shutdown.)
/// * An `Arc`-and-`Mutex`-wrapped `MockCommandCreator` which the server will
///   use for all process creation.
/// * The `JoinHandle` for the server thread.
fn run_server_thread<T>(
    cache_dir: &Path,
    options: T,
) -> (
    crate::net::SocketAddr,
    Sender<ServerMessage>,
    Arc<Mutex<MockCommandCreator>>,
    thread::JoinHandle<()>,
)
where
    T: Into<Option<ServerOptions>> + Send + 'static,
{
    let options = options.into();
    let cache_dir = cache_dir.to_path_buf();

    let cache_size = options
        .as_ref()
        .and_then(|o| o.cache_size.as_ref())
        .copied()
        .unwrap_or(u64::MAX);
    // Create a server on a background thread, get some useful bits from it.
    let (tx, rx) = mpsc::channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let handle = thread::spawn(move || {
        let runtime = Runtime::new().unwrap();
        let dist_client = DistClientContainer::new_disabled();
        let storage = Arc::new(DiskCache::new(
            &cache_dir,
            cache_size,
            runtime.handle(),
            PreprocessorCacheModeConfig::default(),
            CacheMode::ReadWrite,
            vec![],
        ));

        let client = Client::new();
        let srv = SccacheServer::new(0, runtime, client, dist_client, storage).unwrap();
        let mut srv: SccacheServer<_, Arc<Mutex<MockCommandCreator>>> = srv;
        let addr = srv.local_addr().unwrap();
        assert!(matches!(addr, crate::net::SocketAddr::Net(a) if a.port() > 0));
        if let Some(options) = options
            && let Some(timeout) = options.idle_timeout
        {
            srv.set_idle_timeout(Duration::from_millis(timeout));
        }
        let creator = srv.command_creator().clone();
        tx.send((addr, creator)).unwrap();
        srv.run(shutdown_rx).unwrap();
    });
    let (addr, creator) = rx.recv().unwrap();
    (addr, shutdown_tx, creator, handle)
}

#[test]
fn test_server_shutdown() {
    let f = TestFixture::new();
    let (addr, _sender, _storage, child) = run_server_thread(f.tempdir.path(), None);
    // Connect to the server.
    let conn = connect_to_server(&addr).unwrap();
    // Ask it to shut down
    request_shutdown(conn).unwrap();
    // Ensure that it shuts down.
    child.join().unwrap();
}

/// The server will shutdown when requested when the idle timeout is disabled.
#[test]
fn test_server_shutdown_no_idle() {
    let f = TestFixture::new();
    // Set a ridiculously low idle timeout.
    let (addr, _sender, _storage, child) = run_server_thread(
        f.tempdir.path(),
        ServerOptions {
            idle_timeout: Some(0),
            ..Default::default()
        },
    );

    let conn = connect_to_server(&addr).unwrap();
    request_shutdown(conn).unwrap();
    child.join().unwrap();
}

#[test]
fn test_server_idle_timeout() {
    let f = TestFixture::new();
    // Set a ridiculously low idle timeout.
    let (_port, _sender, _storage, child) = run_server_thread(
        f.tempdir.path(),
        ServerOptions {
            idle_timeout: Some(1),
            ..Default::default()
        },
    );
    // Don't connect to it.
    // Ensure that it shuts down.
    // It would be nice to have an explicit timeout here so we don't hang
    // if something breaks...
    child.join().unwrap();
}

#[test]
fn test_server_stats() {
    let f = TestFixture::new();
    let (addr, sender, _storage, child) = run_server_thread(f.tempdir.path(), None);
    // Connect to the server.
    let conn = connect_to_server(&addr).unwrap();
    // Ask it for stats.
    let info = request_stats(conn).unwrap();
    assert_eq!(0, info.stats.compile_requests);
    // Include sccache ver (cli) to validate.
    assert_eq!(env!("CARGO_PKG_VERSION"), info.version);
    // Now signal it to shut down.
    sender.send(ServerMessage::Shutdown).ok().unwrap();
    // Ensure that it shuts down.
    child.join().unwrap();
}

#[test]
fn test_server_unsupported_compiler() {
    let f = TestFixture::new();
    let (addr, sender, server_creator, child) = run_server_thread(f.tempdir.path(), None);
    // Connect to the server.
    let conn = connect_to_server(&addr).unwrap();
    {
        let mut c = server_creator.lock().unwrap();
        // fail rust driver check
        c.next_command_spawns(Ok(MockChild::new(exit_status(1), "hello", "error")));
        // The server will check the compiler, so pretend to be an unsupported
        // compiler.
        c.next_command_spawns(Ok(MockChild::new(exit_status(0), "hello", "error")));
    }
    // Ask the server to compile something.
    //TODO: MockCommand should validate these!
    let exe = &f.bins[0];
    let cmdline = vec!["-c".into(), "file.c".into(), "-o".into(), "file.o".into()];
    let cwd = f.tempdir.path();
    // This creator shouldn't create any processes. It will assert if
    // it tries to.
    let client_creator = new_creator();
    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());
    let path = Some(f.paths);
    let res = do_compile(
        || Ok((client_creator, Runtime::new().unwrap())),
        conn,
        exe,
        cmdline,
        cwd,
        path,
        vec![],
        &mut stdout,
        &mut stderr,
    );
    match res {
        Ok(_) => panic!("do_compile should have failed!"),
        Err(e) => assert_eq!("Compiler not supported: \"error\"", e.to_string()),
    }
    // Make sure we ran the mock processes.
    assert_eq!(0, server_creator.lock().unwrap().children.len());
    // Shut down the server.
    sender.send(ServerMessage::Shutdown).ok().unwrap();
    // Ensure that it shuts down.
    child.join().unwrap();
}

#[test]
fn test_server_compile() {
    let _ = env_logger::try_init();
    let f = TestFixture::new();
    let gcc = f.mk_bin("gcc").unwrap();
    let (addr, sender, server_creator, child) = run_server_thread(f.tempdir.path(), None);
    // Connect to the server.
    const PREPROCESSOR_STDOUT: &[u8] = b"preprocessor stdout";
    const PREPROCESSOR_STDERR: &[u8] = b"preprocessor stderr";
    const STDOUT: &[u8] = b"some stdout";
    const STDERR: &[u8] = b"some stderr";
    let conn = connect_to_server(&addr).unwrap();
    // Write a dummy input file so the preprocessor cache mode can work
    std::fs::write(f.tempdir.path().join("file.c"), "whatever").unwrap();
    {
        let mut c = server_creator.lock().unwrap();
        // The server will check the compiler. Pretend it's GCC.
        c.next_command_spawns(Ok(MockChild::new(exit_status(0), "compiler_id=gcc", "")));
        // Preprocessor invocation.
        c.next_command_spawns(Ok(MockChild::new(
            exit_status(0),
            PREPROCESSOR_STDOUT,
            PREPROCESSOR_STDERR,
        )));
        // Compiler invocation.
        //TODO: wire up a way to get data written to stdin.
        let obj = f.tempdir.path().join("file.o");
        c.next_command_calls(move |_| {
            // Pretend to compile something.
            let mut f = File::create(&obj)?;
            f.write_all(b"file contents")?;
            Ok(MockChild::new(exit_status(0), STDOUT, STDERR))
        });
    }
    // Ask the server to compile something.
    //TODO: MockCommand should validate these!
    let exe = &gcc;
    let cmdline = vec!["-c".into(), "file.c".into(), "-o".into(), "file.o".into()];
    let cwd = f.tempdir.path();
    // This creator shouldn't create any processes. It will assert if
    // it tries to.
    let client_creator = new_creator();
    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());
    let path = Some(f.paths);
    assert_eq!(
        0,
        do_compile(
            || Ok((client_creator, Runtime::new().unwrap())),
            conn,
            exe,
            cmdline,
            cwd,
            path,
            vec![],
            &mut stdout,
            &mut stderr
        )
        .unwrap()
    );
    // Make sure we ran the mock processes.
    assert_eq!(0, server_creator.lock().unwrap().children.len());
    assert_eq!(STDOUT, stdout.into_inner().as_slice());
    assert_eq!(STDERR, stderr.into_inner().as_slice());
    // Shut down the server.
    sender.send(ServerMessage::Shutdown).ok().unwrap();
    // Ensure that it shuts down.
    child.join().unwrap();
}

/// `SCCACHE_MISS_LOG` must be read from the environment the *build* had, not
/// the daemon's.
///
/// The daemon is started by whichever build got there first and then outlives
/// it, so a variable read with `env::var` does nothing unless that shell
/// happened to start the server -- and stops working after an idle timeout.
/// `933ec5e` fixed exactly this for `SCCACHE_CACHE_MULTIARCH`, and its tests
/// had survived the bug by setting the variable the way the code read it rather
/// than the way a build sets it. This one passes it as the compilation's
/// environment, and fails against a daemon-side read.
#[test]
fn test_miss_log_comes_from_the_compilation_environment() {
    let _ = env_logger::try_init();
    let f = TestFixture::new();
    let gcc = f.mk_bin("gcc").unwrap();
    let (addr, sender, server_creator, child) = run_server_thread(f.tempdir.path(), None);
    let conn = connect_to_server(&addr).unwrap();
    std::fs::write(f.tempdir.path().join("file.c"), "whatever").unwrap();

    let miss_log = f.tempdir.path().join("misses.tsv");
    assert!(
        crate::envvar::var_os("SCCION_MISS_LOG").is_none(),
        "this test is only meaningful while the daemon's own environment is silent"
    );

    {
        let mut c = server_creator.lock().unwrap();
        c.next_command_spawns(Ok(MockChild::new(exit_status(0), "compiler_id=gcc", "")));
        c.next_command_spawns(Ok(MockChild::new(exit_status(0), "preprocessed", "")));
        let obj = f.tempdir.path().join("file.o");
        c.next_command_calls(move |_| {
            let mut f = File::create(&obj)?;
            f.write_all(b"file contents")?;
            Ok(MockChild::new(exit_status(0), "", ""))
        });
    }

    let cmdline = vec!["-c".into(), "file.c".into(), "-o".into(), "file.o".into()];
    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());
    assert_eq!(
        0,
        do_compile(
            || Ok((new_creator(), Runtime::new().unwrap())),
            conn,
            &gcc,
            cmdline,
            f.tempdir.path(),
            Some(f.paths),
            vec![("SCCACHE_MISS_LOG".into(), miss_log.clone().into())],
            &mut stdout,
            &mut stderr
        )
        .unwrap()
    );

    sender.send(ServerMessage::Shutdown).ok().unwrap();
    child.join().unwrap();

    let text = std::fs::read_to_string(&miss_log)
        .expect("the compilation named a miss log and missed, so there must be one");
    let lines: Vec<_> = text.lines().collect();
    // One compilation, one record. This catches the record being written both
    // at the lookup and again after the compile; it does not reach the case the
    // deferral exists for, which is a miss whose *compile* then proves it
    // permanently uncacheable and must reclassify the record rather than add a
    // second one. That needs a compiler mock that returns `Cacheable::No`.
    assert_eq!(lines.len(), 1, "a single compilation wrote: {text}");
    let cols: Vec<_> = lines[0].split('\t').collect();
    assert_eq!(cols[0], "cold", "a cold cache is not a mystery");
    assert_eq!(cols[3], "file.o");
    assert_eq!(
        cols[4], "file.c",
        "the input, which is what tells two identically-named outputs apart"
    );
}

/// Two callers, two buckets, and the buckets add up to the machine totals.
///
/// One daemon serves every build on the machine, so its numbers are a sum with
/// no addends: "2557 hits" does not say whether one caller got all of them and
/// another none. The per-caller breakdown has to be a breakdown -- a second,
/// independent accounting that disagreed with the totals would be worse than
/// none.
#[test]
fn test_stats_break_down_per_caller() {
    let _ = env_logger::try_init();
    let f = TestFixture::new();
    let gcc = f.mk_bin("gcc").unwrap();
    let (addr, sender, server_creator, child) = run_server_thread(f.tempdir.path(), None);
    std::fs::write(f.tempdir.path().join("file.c"), "whatever").unwrap();

    for (i, agent) in ["agent-a", "agent-b"].iter().enumerate() {
        {
            let mut c = server_creator.lock().unwrap();
            if i == 0 {
                // The compiler is probed once, for the first request.
                c.next_command_spawns(Ok(MockChild::new(exit_status(0), "compiler_id=gcc", "")));
            }
            c.next_command_spawns(Ok(MockChild::new(exit_status(0), "preprocessed", "")));
            let obj = f.tempdir.path().join(format!("file{i}.o"));
            c.next_command_calls(move |_| {
                let mut f = File::create(&obj)?;
                f.write_all(b"file contents")?;
                Ok(MockChild::new(exit_status(0), "", ""))
            });
        }
        let conn = connect_to_server(&addr).unwrap();
        let cmdline = vec![
            "-c".into(),
            "file.c".into(),
            "-o".into(),
            format!("file{i}.o").into(),
        ];
        let mut stdout = Cursor::new(Vec::new());
        let mut stderr = Cursor::new(Vec::new());
        assert_eq!(
            0,
            do_compile(
                || Ok((new_creator(), Runtime::new().unwrap())),
                conn,
                &gcc,
                cmdline,
                f.tempdir.path(),
                Some(f.paths.clone()),
                vec![("SCCACHE_AGENT_ID".into(), (*agent).into())],
                &mut stdout,
                &mut stderr
            )
            .unwrap()
        );
    }

    let stats = request_stats(connect_to_server(&addr).unwrap())
        .unwrap()
        .stats;
    sender.send(ServerMessage::Shutdown).ok().unwrap();
    child.join().unwrap();

    let a = stats.per_agent.get("agent-a").expect("agent-a");
    let b = stats.per_agent.get("agent-b").expect("agent-b");
    assert_eq!(
        stats.per_agent.len(),
        2,
        "one bucket each, not a shared one"
    );
    assert_eq!(a.requests, 1);
    assert_eq!(b.requests, 1);
    assert_eq!(
        a.cache_misses + b.cache_misses,
        stats.cache_misses.all(),
        "the breakdown has to add up to the total it breaks down"
    );
    assert_eq!(a.cache_hits + b.cache_hits, stats.cache_hits.all());
}

/// A handle names a workspace, and is the same handle next time.
///
/// Minting a fresh one per call would let a caller grow the daemon's table
/// without limit, and would mean an agent that reconnects loses the identity it
/// was using -- there are no sessions to hang it on, so the handle *is* the
/// continuity.
#[test]
fn test_a_workspace_handle_is_stable_and_scopes_what_it_can_see() {
    let f = TestFixture::new();
    let checkout = f.tempdir.path().join("checkout");
    std::fs::create_dir_all(checkout.join("src")).unwrap();
    std::fs::create_dir_all(checkout.join(".git")).unwrap();
    // A second checkout of the same project, which is the case all of this
    // exists for.
    let other_checkout = f.tempdir.path().join("worktree");
    std::fs::create_dir_all(&other_checkout).unwrap();
    std::fs::write(other_checkout.join(".git"), "gitdir: ../checkout/.git").unwrap();
    let elsewhere = f.tempdir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();

    let (addr, sender, _creator, child) = run_server_thread(f.tempdir.path(), None);

    let mut conn = connect_to_server(&addr).unwrap();
    let first = match conn
        .request(crate::protocol::Request::RegisterWorkspace {
            path: checkout.join("src").into_os_string(),
        })
        .unwrap()
    {
        crate::protocol::Response::Workspace(Ok(info)) => info,
        other => panic!("unexpected response: {other:?}"),
    };
    assert_eq!(
        first.root,
        crate::agent::canonical(&checkout),
        "a directory inside a checkout registers the checkout, not itself -- \
         in the one spelling everything else compares against"
    );
    assert_eq!(first.vcs, "git");
    assert!(
        first.handle.len() >= 32,
        "a handle is a capability, so it has to be unguessable: {:?}",
        first.handle
    );

    // The same checkout, reached from a different directory inside it.
    let again = match conn
        .request(crate::protocol::Request::RegisterWorkspace {
            path: checkout.clone().into_os_string(),
        })
        .unwrap()
    {
        crate::protocol::Response::Workspace(Ok(info)) => info,
        other => panic!("unexpected response: {other:?}"),
    };
    assert_eq!(again.handle, first.handle, "same checkout, same handle");

    // A different checkout is a different handle.
    let other = match conn
        .request(crate::protocol::Request::RegisterWorkspace {
            path: other_checkout.into_os_string(),
        })
        .unwrap()
    {
        crate::protocol::Response::Workspace(Ok(info)) => info,
        other => panic!("unexpected response: {other:?}"),
    };
    assert_ne!(other.handle, first.handle);

    // A directory outside any checkout is refused rather than given a handle
    // that no record could ever match -- which would answer every question
    // with an empty list, reading as "nothing missed" instead of "not a
    // workspace".
    match conn
        .request(crate::protocol::Request::RegisterWorkspace {
            path: elsewhere.into_os_string(),
        })
        .unwrap()
    {
        crate::protocol::Response::Workspace(Err(_)) => {}
        other => panic!("a non-checkout must not register: {other:?}"),
    }

    // And an invented handle is refused rather than answered broadly.
    match conn
        .request(crate::protocol::Request::GetMissRecords {
            handle: Some("0".repeat(32)),
            limit: 10,
        })
        .unwrap()
    {
        crate::protocol::Response::MissRecords(Err(_)) => {}
        other => panic!("an unknown handle must not be answered: {other:?}"),
    }

    // The shutdown waits for open connections, and this one is deliberately
    // reused across every request above rather than opened per call.
    drop(conn);
    sender.send(ServerMessage::Shutdown).ok().unwrap();
    child.join().unwrap();
}

/// A checkout that declines MCP is refused a handle, which closes every tool.
///
/// Registration is the single gate on purpose: every other tool needs a handle,
/// so there is one place to enforce this rather than five, and refusing outright
/// beats answering emptily -- an empty answer reads as "nothing to report".
#[test]
fn test_a_checkout_can_decline_to_be_answered_for_over_mcp() {
    let f = TestFixture::new();
    let checkout = f.tempdir.path().join("private");
    std::fs::create_dir_all(checkout.join(".git")).unwrap();
    std::fs::write(
        checkout.join(crate::projectconf::PROJECT_CONF_NAME),
        "expose_over_mcp = false\n",
    )
    .unwrap();

    let (addr, sender, _creator, child) = run_server_thread(f.tempdir.path(), None);
    let mut conn = connect_to_server(&addr).unwrap();

    let r = conn
        .request(crate::protocol::Request::RegisterWorkspace {
            path: checkout.clone().into_os_string(),
        })
        .unwrap();
    match r {
        crate::protocol::Response::Workspace(Err(msg)) => {
            assert!(
                msg.contains("expose_over_mcp"),
                "the refusal has to say which setting caused it: {msg}"
            );
        }
        other => panic!("a declining checkout must not get a handle: {other:?}"),
    }

    drop(conn);
    sender.send(ServerMessage::Shutdown).ok().unwrap();
    child.join().unwrap();
}

/// A compilation sccache refuses before parsing used to produce no record at
/// all. It is the most important kind to record: nothing about it will ever be
/// cached, so a caller that treats it as an ordinary miss retries forever.
///
/// `/dynamicdeopt` reached this path for 100% of a real build's translation
/// units, and the counters could say only that it had happened.
#[test]
fn test_a_refusal_before_parsing_is_recorded() {
    let _ = env_logger::try_init();
    let f = TestFixture::new();
    let gcc = f.mk_bin("gcc").unwrap();
    let (addr, sender, server_creator, child) = run_server_thread(f.tempdir.path(), None);
    let conn = connect_to_server(&addr).unwrap();

    let miss_log = f.tempdir.path().join("misses.tsv");
    {
        let mut c = server_creator.lock().unwrap();
        c.next_command_spawns(Ok(MockChild::new(exit_status(0), "compiler_id=gcc", "")));
    }

    // A refused compilation comes back as `UnhandledCompile` and the client
    // runs the compiler itself, so it needs a process of its own to run.
    let client_creator = new_creator();
    next_command(&client_creator, Ok(MockChild::new(exit_status(0), "", "")));

    // `-E` means "preprocess only", which sccache declines outright.
    let cmdline = vec!["-c".into(), "-E".into(), "file.c".into()];
    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());
    let _ = do_compile(
        || Ok((client_creator, Runtime::new().unwrap())),
        conn,
        &gcc,
        cmdline,
        f.tempdir.path(),
        Some(f.paths),
        vec![("SCCACHE_MISS_LOG".into(), miss_log.clone().into())],
        &mut stdout,
        &mut stderr,
    );

    sender.send(ServerMessage::Shutdown).ok().unwrap();
    child.join().unwrap();

    let text =
        std::fs::read_to_string(&miss_log).expect("a refusal is a miss, and a permanent one");
    let cols: Vec<_> = text
        .lines()
        .next()
        .expect("one record")
        .split('\t')
        .collect();
    assert!(
        cols[0].starts_with("uncacheable:"),
        "the classification has to say `stop`, not `try again`: {:?}",
        cols[0]
    );
    assert_eq!(cols[2], "-", "nothing got far enough to compute a key");
    assert!(
        cols[8] != "-",
        "and the command digest is what is left to identify it by"
    );
}
