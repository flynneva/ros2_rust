use anyhow::{Error, Result};
use std::env;

fn main() -> Result<(), Error> {
    let context = rclrs::Context::new(env::args()).unwrap();

    let mut node = context.create_node("minimal_client")?;

    let client = node.create_client::<example_interfaces::srv::AddTwoInts>("add_two_ints")?;

    let mut request = example_interfaces::srv::AddTwoInts_Request::default();
    request.a = 41;
    request.b = 1;

    println!("Starting client");

    std::thread::sleep(std::time::Duration::from_millis(500));

    let future = client.call_async(&request)?;

    println!("Waiting for response");
    let response = rclrs::spin_until_future_complete(&node, future.clone())?;

    println!(
        "Result of {} + {} is: {}",
        request.a, request.b, response.sum
    );
    Ok(())
}
