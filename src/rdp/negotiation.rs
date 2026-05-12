/// Performs the RDP connection negotiation sequence and returns the
/// negotiated desktop (width, height).
///
/// RDP handshake order:
///   1. X.224 Connection Request  (client → server)
///   2. X.224 Connection Confirm  (server → client)
///   3. MCS Connect-Initial       (client → server)
///   4. MCS Connect-Response      (server → client, carries server capabilities)
///   5. MCS Erect Domain / Attach User
///   6. Security Exchange / Client Info PDU
///   7. Server Licence / Demand Active / Confirm Active
///
/// Phase 1 stub: return a fixed resolution so the rest of the pipeline
/// can compile and run. Replace with real ironrdp-acceptor calls.
pub async fn perform<S>(_stream: &S) -> anyhow::Result<(u16, u16)> {
    // TODO: drive ironrdp_acceptor::Acceptor state machine over `stream`
    Ok((1920, 1080))
}
