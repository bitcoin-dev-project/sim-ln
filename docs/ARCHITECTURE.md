# Sim-Ln

## Simulation Stages

Simulation activities are broken down into four stages:

1. Simulation Event: an action that the simulator should execute,
   produced from the user-provided description of desired activities.
2. Simulation Output: the output of executing an simulation event. Note
   that this is not the final outcome of the event, as the event itself
   may take a non-trivial amount of time to complete.
3. Result Tracking: per-output tracking of the final outcome of the
   simulation event.
4. Simulation Result: recording of the final result as provided by
   result tracking, which is a final result of the simulation.

## Activity Generation

The simulator uses tokio's asynchronous multi-producer, single-consumer
channels to implement a consume/producer architecture to produce,
execute and track simulation events. Stages of the simulation
communicate by passing the _receiver_ for the next stage to the
consumer from the previous stage, allowing each stage to pass it's
output on to the next one.

```
          +-----------------+
          |  EventProducer  |
          +-----------------+
                   |
            SimulationEvent
                   |
                   v
          +-----------------+
          |  EventConsumer  |
          +-----------------+
                   |
            SimulationOutput
                   |
                   v
          +-----------------+                       +-----------------+
          | ResultsProducer | --SimulationOutput--> |   TrackResult   |
          +-----------------+                       +-----------------+
                                                             |
                                                       PaymentOutcome
                                                             |
                                                             v
                                                    +-----------------+
                                                    | ResultsConsumer |
                                                    +-----------------+
                                                            |
                                                            v
                                                       simulation.csv
```

### Shutdown

To ensure that all tasks shutdown on completion (or failure) of the
simulation, we relay on the following:

1. [Triggered](https://docs.rs/triggered/latest/triggered): a `Trigger`
   that can be used to inform threads/tasks that it's time to shut down,
   and a `Listener` that propagates this signal.
2. The (`Trigger`, `Listener`) pair are used with channels: if a channel
 errors out across `send()` or `recv()`, shutdown is triggered. There is
 no reliance on channel mechanics, i.e. that receiving channels will error
 out when all of their sending channels are dropped.
3. All events are handled in a `tokio::select` to allow waiting on
multiple asynchronous tasks at once. These selects should be `biased`
on the exit case (ie, the `Listener` being triggered) so that we
prioritize exit above generating more events.
4. Additionally, we `select!` on shutdown signal on `send()`/`recv()`
for all channels to guarantee shutdown:
  - When the signal to shutdown is received, it is possible that a 
    task responsible for consuming on the `Receiver` channel exits when 
    multiple tasks are still attempting to send to it. 
  - By using `select` with all `send()` instructions, we ensure that 
    the senders will exit cleanly, rather than block waiting on a 
    receiver that has already exited to consume its send.
  - An alternative to this approach would be to use `receiver.close()` 
    and drain all items from the channel (resulting in unblocking the 
    senders). 
