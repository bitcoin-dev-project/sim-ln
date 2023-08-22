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
  that can be used to inform threads that it's time to shut down, and 
  a `Listener` that propagates this signal.
2. Channel mechanics: if all of the senders for a channel have been 
  dropped (in our context, all of the producers have exited) then the 
  corresponding receiving channel will error out on receiving. Likewise, 
  if the receiving channel is dropped, the sending channels will error 
  out.

All events are handled in a `tokio::select` to allow waiting on 
multiple asynchronous tasks at once. These selects should be `biased` 
on the exit case (ie, the `Listener` being triggered) so that we 
prioritize exit above generating more events.

Practically, this means that we're handling the various shutdown 
scenarios in the following way: 

1. Consumer error: 
  - Receiving channel is dropped on consumer exit.
  - Sending channels used by producers will error out. 
  - Producers will trigger shutdown.
  - Listening producers will exit.

2. Producer error: 
  - Producers will trigger shutdown.
  - Listening producers will exit.
  - Once all producers have exited, the consuming channel's receiver 
    will error out causing it to exit.

3. Miscellaneous tasks: 
  - Trigger shutdown on exit.
  - Listen for shutdown from other errors.

