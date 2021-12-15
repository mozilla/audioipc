# [cubeb](https://github.com/mozilla/cubeb) audio remoting for Gecko

An IPC client/server providing inter-process remoting of the [cubeb audio
API](https://github.com/mozilla/cubeb/blob/master/include/cubeb/cubeb.h).

A single server running in a parent or server process interfaces with cubeb
on behalf of multiple client or child processes.

On the server side, the external interface is a small set of C FFI functions
to initialize and destroy the server
(`audioipc_server_start`/`audioipc_server_stop`) and create new server
connections (`audioipc_server_new_client`).  On the client side, the
external interface consists of a single C FFI function
(`audioipc_client_init`), replacing a call to `cubeb_init` and returning a
cubeb context that transparently remotes the cubeb API to the server; aside
from this the client uses the cubeb API and remoting is handled
transparently.  `audioipc_server_new_client` returns a handle that must be
supplied to `audioipc_client_init` to establish the client-server
connection.  Transferring this handle from the server process to the client
process is done externally, in Gecko code.

- The client side remoting layer (located in the `client` crate) contains
  ClientContext and ClientStream, which adapts the cubeb context and cubeb
  stream APIs (respectively) to the RPC layer.  This is driven by one
  "Client RPC" EventLoopThread per ClientContext and one "Client Callback"
  EventLoopThread per ClientStream.
- The server side remoting layer (located in the `server` crate) contains
  CubebServer, which adapts both context and stream RPCs to native cubeb API
  calls.  This is driven by a single trio of "Server RPC", "Server
  Callback", and "Server DeviceCollection RPC) EventLoopThreads servicing all
  CubebServer instances.
- Core RPC and IPC functionality and support code is located in the `audioipc` crate.
  - The RPC interface provides a Client trait specifying message types the
    client implementation will use, and a Server trait specifying message
    types the server implementation will use and a `process` function that
    handles incoming messages and produces responses.  Server and Client
    implementations work as pairs, with one on each side of the IPC
    connection.  Server and Client implementations are driven by an
    EventLoopThread once bound to the event loop with a connection via
    `bind_server` or `bind_client`.  Each Server implementation may have
    multiple associated Client implementations in one or more remote
    process.
  - The RPC interface also provides Proxy and ProxyResponse objects,
    providing a blocking thread-safe mechanism to send and receive RPC
    messages from arbitrary threads.
  - The IPC interface provides the core EventLoopThread object, responsible
    for driving inter-process communication on each end of previously bound
    Client and Server implementations (and their associated connections).
  - The IPC interface also provides a cloneable, sendable EventLoopHandle
    object associated with an EventLoopThread that allows arbitrary threads
    to bind Client and Server implementations and signal the EventLoopThread
    to shut down.
