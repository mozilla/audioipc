This replaces the existing IPC messaging core built around tokio 0.1 with a new one built around mio 0.7.

The intention of this change is to reduce external dependencies and tighten control over the architecture so that it can be customized for better soft-RT performance for our audio uses.  This will also make it easier to introduce Firefox profiler markers in the IPC code to improve field debugability.

Replacing the old 0.1 IPC core also addresses the various issues that were blocking AudioIPC for macOS in Gecko.

The initial version retains a similar general architecture to the existing IPC code in terms of threading and relationships between server, client, cubeb context and cubeb stream.  With the initial working version in place, the intention is to then take advantage of the new architecture to enable functionality that wasn't easy to provide within the restrictions of the old architecture, including:
    - per-stream callback remoting for high priority/low latency streams
        where each stream may be serviced by it's own thread/EventLoop on the client side
    - more efficient use of shared memory segments, including using shmem for message passing
    - client-side stream multiplexing for low priority/high latency streams
        trading off latency for reduced IPC overhead
    - providing a new API for cubeb stream consumers allowing them to provide their own thread for callback processing,
      with new methods to wait for free buffer space and provide refilled buffers for capture/playback

General architecture:
- Server side:
    - Main "server" thread executing EventLoop, with one connection per client
        - Connections are Unix Domain Sockets on Unix, and Named Pipes on Windows
        - Each connection services one or more remote cubeb streams for the client
    - RPC Proxy, allowing thread-safe message send/receive, used within
        cubeb callbacks to communicate with client-side cubeb streams via EventLoop
- Client side:
    - EventLoop thread with a single connection, communicating with server
    - RPC Proxy per cubeb stream, remoting cubeb API calls to the server (via EventLoop)

TODO:
- Remove existing tokio 0.1 code and deps (once all platforms switched over)
- Delete tokio_named_pipes, tokio_uds_stream
- Delete messagestream_*.rs
- Delete AsyncSendMsg/AsyncRecvMsg traits/impls
