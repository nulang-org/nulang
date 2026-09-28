use std::env;
use std::io::{self, Write};
use std::path::Path;
use std::thread;

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(81).expect("fixture tablet id"),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).expect("fixture key range"),
        5,
    )
    .expect("fixture descriptor")
}

fn acknowledge(message: &str) {
    println!("{message}");
    io::stdout().flush().expect("flush durability acknowledgement");
}

fn park_until_killed() -> ! {
    loop {
        thread::park();
    }
}

fn main() {
    let mut args = env::args_os().skip(1);
    let wal_path = args.next().expect("usage: nudb_crash_fixture <wal> <action>");
    let action = args
        .next()
        .and_then(|value| value.into_string().ok())
        .expect("usage: nudb_crash_fixture <wal> <action>");
    assert!(args.next().is_none(), "unexpected extra fixture arguments");

    let wal_path = Path::new(&wal_path);
    match action.as_str() {
        "commit" => {
            let mut tablet =
                WalBackedTablet::open(descriptor(), wal_path).expect("open fixture tablet");
            let previous = tablet.current_sequence();
            let write = tablet
                .prepare_write(
                    5,
                    previous,
                    vec![TabletMutation::Put {
                        key: b"k".to_vec(),
                        value: b"value".to_vec(),
                    }],
                )
                .expect("prepare fixture write");
            let sequence = tablet.commit(write).expect("commit fixture write");
            acknowledge(&format!("ACK COMMIT {sequence}"));
        }
        "publish-checkpoint" => {
            let tablet =
                WalBackedTablet::open(descriptor(), wal_path).expect("open fixture tablet");
            let sequence = tablet.current_sequence();
            tablet
                .publish_checkpoint()
                .expect("publish fixture checkpoint");
            acknowledge(&format!("ACK CHECKPOINT {sequence}"));
        }
        "checkpoint" => {
            let mut tablet =
                WalBackedTablet::open(descriptor(), wal_path).expect("open fixture tablet");
            let sequence = tablet.current_sequence();
            tablet.checkpoint().expect("checkpoint fixture tablet");
            acknowledge(&format!("ACK CHECKPOINT_RECLAIMED {sequence}"));
        }
        other => panic!("unknown crash fixture action: {other}"),
    }

    // The parent test must terminate us after observing the ACK. Keeping the
    // process alive ensures this exercises abrupt process death instead of
    // normal destructor/drop shutdown.
    park_until_killed();
}
