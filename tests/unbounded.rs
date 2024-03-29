use core::time;
use std::sync::mpsc;

use pochta::registry;

#[test]
fn send_and_unsubscribe() {
    const ID: u8 = 1;
    let (send, recv) = mpsc::channel();

    let (channel, mut registry) = registry();
    let worker = std::thread::spawn(move || {
        registry.run();
    });

    channel.subscribe(ID, send).expect("Success");
    channel.send_to(ID, "test").expect("Success");
    channel.unsubscribe(ID).expect("Success");
    channel.send_to(ID, "test2").expect("Success");

    let message = recv.recv().expect("Success");
    assert_eq!(message, "test");
    //Should be disconnected after unsubscribe
    let message = recv.recv_timeout(time::Duration::from_millis(100));
    assert_eq!(message, Err(mpsc::RecvTimeoutError::Disconnected));

    drop(channel.clone());
    drop(channel);

    worker.join().expect("Finish successfully");
}
