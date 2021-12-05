# `exit-left`

> *verb;*
>
> *1. To exit or disappear in a quiet, non-dramatic fashion, making way for more interesting events.*
>
> *2. (imperative) Leave the scene, and don't make a fuss.*

A rust synchronization primitive that can allow multiplexing of an underlying resource.

The idea is as such; You have many threads, all of them want to "wait" for some event to come through the pipeline,
but the pipeline can give all kinds events in any order.

Worse still, only one thread can poll/block on this resource, and could have to deal with the unwanted events before it gets the one it would want.

This crate's `Stage` struct is the solution, any thread "heading" it would be responsible for routing the events,
but it would be provided the tools to easily alert other threads waiting for "it's own" resource.

It does this via a `Hash + Eq` token type, which is passed to a `ticket` function, registering the token to be activated. Following this, the thread can then
investigate the buffer, if it finds nothing, it can `enter` the stage, after which it will resume under one of the following three conditions;

1. The thread is promoted to "heading" the stage, this can happen after the previous heading stage exits the stage, or if this thread is the first one to "enter" it.
2. The thread passes a `timeout` parameter, and it passes by, the thread will then resume.
3. A "heading" thread will notify this thread via its token, the thread will then resume.

### Usage

A potential usage would be to poll a `UdpSocket`, which can receive packets from multiple sources, but also send to multiple.

Unlike a `TcpStream`, a `UdpSocket` has two potential modes, "Connected", or "Not Connected".

The former merely meaning that a filter is in place for a particular source, and all other packets are dropped.

Thus, normally, a `UdpSocket` in only "Connected" states would have to be constructed again and again for every local-remote tuple it would be used for,
with local ports becoming unaccessible to the rest of the system after its been `bind`-ed.

So, the `Stage` can help many threads to await a socket at the same time, while only one does the actual routing work,
while still leaving the ability to notify threads waiting for "their" endpoint.
