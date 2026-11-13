import Foundation
import Network

/// TCP + TLS byte streams for the core's Connect Anywhere client (HTTPS and
/// a WebSocket to the rendezvous service). The core calls these blocking
/// functions on its own background threads.
enum Transport {
    static func install() {
        sard_set_transport(SardTransport(
            open: { host, port, tls, timeout in
                guard let host, let p = NWEndpoint.Port(rawValue: port) else { return nil }
                let conn = StreamConnection(host: String(cString: host), port: p, tls: tls)
                guard conn.open(timeout: timeout) else { return nil }
                return Unmanaged.passRetained(conn).toOpaque()
            },
            write: { ptr, data, len, timeout in
                guard let ptr, let data else { return -1 }
                let conn = Unmanaged<StreamConnection>.fromOpaque(ptr).takeUnretainedValue()
                return conn.write(Data(bytes: data, count: len), timeout: timeout)
            },
            read: { ptr, buf, len, timeout in
                guard let ptr, let buf else { return -1 }
                let conn = Unmanaged<StreamConnection>.fromOpaque(ptr).takeUnretainedValue()
                return conn.read(into: buf, max: len, timeout: timeout)
            },
            close: { ptr in
                guard let ptr else { return }
                let conn = Unmanaged<StreamConnection>.fromOpaque(ptr).takeRetainedValue()
                conn.close()
            }))
    }
}

private final class StreamConnection {
    private let connection: NWConnection
    private let queue = DispatchQueue(label: "sard.transport")
    private let lock = NSLock()
    private var buffer = Data()
    private var finished = false
    private var failed = false
    private var receiving = false
    private let arrived = DispatchSemaphore(value: 0)

    init(host: String, port: NWEndpoint.Port, tls: Bool) {
        connection = NWConnection(host: NWEndpoint.Host(host), port: port, using: tls ? .tls : .tcp)
    }

    private static func deadline(_ ms: Int32) -> DispatchTime {
        ms < 0 ? .distantFuture : .now() + .milliseconds(Int(ms))
    }

    func open(timeout: Int32) -> Bool {
        let ready = DispatchSemaphore(value: 0)
        var ok = false
        connection.stateUpdateHandler = { state in
            switch state {
            case .ready:
                ok = true
                ready.signal()
            case .failed, .cancelled:
                ready.signal()
            case .waiting:
                // No route right now (offline): give up rather than wait.
                ready.signal()
            default: break
            }
        }
        connection.start(queue: queue)
        _ = ready.wait(timeout: Self.deadline(timeout))
        connection.stateUpdateHandler = nil
        if !ok { connection.cancel() }
        return ok
    }

    func write(_ data: Data, timeout: Int32) -> Int {
        let done = DispatchSemaphore(value: 0)
        var error: NWError?
        connection.send(content: data, completion: .contentProcessed { e in
            error = e
            done.signal()
        })
        if done.wait(timeout: Self.deadline(timeout)) == .timedOut { return -2 }
        return error == nil ? data.count : -1
    }

    /// One receive at a time; a receive that outlives a read's timeout still
    /// lands in `buffer` for the next read (nothing is lost).
    private func receiveMore() {
        lock.lock()
        if receiving || finished || failed {
            lock.unlock()
            return
        }
        receiving = true
        lock.unlock()
        connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) { [weak self] data, _, complete, error in
            guard let self else { return }
            self.lock.lock()
            if let data { self.buffer.append(data) }
            if complete { self.finished = true }
            if error != nil { self.failed = true }
            self.receiving = false
            self.lock.unlock()
            self.arrived.signal()
        }
    }

    func read(into buf: UnsafeMutablePointer<UInt8>, max: Int, timeout: Int32) -> Int {
        let deadline = Self.deadline(timeout)
        while true {
            lock.lock()
            if !buffer.isEmpty {
                let n = min(max, buffer.count)
                buffer.copyBytes(to: buf, count: n)
                buffer.removeFirst(n)
                lock.unlock()
                return n
            }
            if failed { lock.unlock(); return -1 }
            if finished { lock.unlock(); return 0 }
            lock.unlock()
            receiveMore()
            if arrived.wait(timeout: deadline) == .timedOut { return -2 }
        }
    }

    func close() {
        connection.cancel()
    }
}
