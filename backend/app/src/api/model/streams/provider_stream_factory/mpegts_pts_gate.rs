//! Drops the replayed prefix of a reopened live MPEG-TS episode and keeps the
//! delivered transport stream continuous from the player's point of view.
//!
//! Some panels answer a channel with a body that ends after a few megabytes and serve the next
//! request from a few seconds **behind** live edge, racing forward until it catches up and closing
//! again. Splicing such episodes back to back replays content the viewer already received and
//! throws the timestamps backwards at every boundary. Players treat that backward jump as clock
//! jitter and start discarding pictures, which the viewer sees as a frozen frame.
//!
//! Three things must hold across a splice, and the player reacts to all of them:
//!
//! 1. **Presentation timestamps** must not go backwards. The gate keeps the last PTS emitted per
//!    PID and holds a PID back until its timestamps have advanced past what was delivered.
//! 2. **PCR must not go backwards.** Players derive their clock from PCR, not PTS. Dropping the
//!    replayed PES leaves the embedded PCR of the new episode slightly behind the one already
//!    delivered, which libavformat reports as a timestamp discontinuity and resolves by jumping
//!    its clock — the visible time jump. The gate therefore withholds (or strips) PCR values that
//!    are not ahead of the last one emitted.
//! 3. **Continuity counters must not break.** A counter break is a declared discontinuity, so
//!    deleting packets must never leave a gap. Because the provider also restarts each episode
//!    from its own counter sequence, the gate renumbers every emitted packet to a continuous
//!    per-PID sequence.
//!
//! Streams that are not transport stream at all (no `0x47` sync, no PES) never lock the gate and
//! pass through unchanged, so the wrapper keeps working for any provider.

use std::collections::HashMap;

const TS_PACKET_SIZE: usize = 188;
const SYNC_BYTE: u8 = 0x47;
const NULL_PID: u16 = 0x1FFF;
/// Backward tolerance: a PES PID may reopen slightly behind its last emitted timestamp without
/// being considered overlapped (encoder jitter between episodes).
const REOPEN_BACKWARD_TOLERANCE_TICKS: u64 = 9_000; // 100 ms in 90 kHz ticks
const PTS_MODULUS: u64 = 1 << 33;
const PTS_HALF_RANGE: u64 = 1 << 32;
/// PCR runs at 27 MHz; its base field is the same 33-bit wrapping counter as PTS.
const PCR_MODULUS: u64 = (1 << 33) * 300;
const PCR_HALF_RANGE: u64 = PCR_MODULUS / 2;
/// A PCR may repeat or jitter by a tick or two without being a discontinuity.
const PCR_BACKWARD_TOLERANCE: u64 = 300; // 1/90000 s

#[derive(Default)]
struct PidGate {
    /// Last presentation timestamp emitted for this pid, in 90 kHz ticks.
    last_emitted_pts: u64,
    /// Whether this episode's packets for this pid may pass through yet.
    open: bool,
    /// Last PCR emitted for this pid, in 27 MHz ticks.
    last_emitted_pcr: Option<u64>,
}

/// Stateful per-pid overlap filter for one live MPEG-TS response.
#[derive(Default)]
pub(super) struct MpegTsPtsGate {
    buffer: Vec<u8>,
    /// Pids that carried a PES presentation timestamp at least once.
    pids: HashMap<u16, PidGate>,
    /// Last continuity counter emitted per pid, so deletions never create a gap.
    continuity: HashMap<u16, u8>,
    /// Set once a packet yielded a presentation timestamp; until then everything passes through.
    locked: bool,
}

impl MpegTsPtsGate {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Starts gating the next episode: each pid must again prove its timestamps moved forward.
    pub(super) fn begin_new_episode(&mut self) {
        for gate in self.pids.values_mut() {
            gate.open = false;
        }
    }

    /// Feeds one upstream chunk and appends the bytes that may still be delivered.
    pub(super) fn push_chunk(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        self.buffer.extend_from_slice(chunk);
        while !self.buffer.is_empty() {
            if self.buffer[0] != SYNC_BYTE {
                // Before the first presentation timestamp there is no proof this is a transport
                // stream, so unknown bytes are passed through instead of silently dropped.
                let byte = self.buffer.remove(0);
                if !self.locked {
                    out.push(byte);
                }
                continue;
            }
            if self.buffer.len() < TS_PACKET_SIZE {
                // A partial packet stays buffered until its remainder arrives.
                break;
            }
            let mut packet: Vec<u8> = self.buffer.drain(..TS_PACKET_SIZE).collect();
            if self.packet_may_pass(&mut packet) {
                out.extend_from_slice(&packet);
            }
        }
    }

    /// Whether one transport packet belongs after everything already emitted.
    fn packet_may_pass(&mut self, packet: &mut [u8]) -> bool {
        let pid = (((packet[1] & 0x1F) as u16) << 8) | packet[2] as u16;
        let payload_unit_start = packet[1] & 0x40 != 0;
        let adaptation_field_control = (packet[3] >> 4) & 0x3;
        let has_payload = adaptation_field_control & 0b01 != 0;
        let mut payload_start = 4;
        if adaptation_field_control & 0b10 != 0 {
            let Some(&adaptation_length) = packet.get(4) else { return !self.locked };
            payload_start = 5 + adaptation_length as usize;
        }

        if !has_payload {
            // A PCR-only packet carries no picture: it follows its pid's verdict, and only its
            // clock value decides whether it can still be delivered.
            if !self.pid_may_pass(pid) {
                return false;
            }
            return match Self::pcr_of(packet) {
                Some(pcr) if self.pcr_is_behind(pid, pcr) => false,
                Some(pcr) => {
                    self.remember_pcr(pid, pcr);
                    self.renumber_continuity(packet, pid, false);
                    true
                }
                None => {
                    self.renumber_continuity(packet, pid, false);
                    true
                }
            };
        }

        let pts = if payload_unit_start { parse_pes_pts(packet, payload_start) } else { None };
        let passes = match pts {
            Some(pts) => {
                self.locked = true;
                let gate = self.pids.entry(pid).or_insert(PidGate { last_emitted_pts: pts, open: true, ..PidGate::default() });
                if gate.open && pts_is_behind(pts, gate.last_emitted_pts) {
                    // A mid-body rewind from the provider is the same replay as an episode
                    // boundary: hold this pid back until its timestamps catch up again.
                    gate.open = false;
                }
                if gate.open || !pts_is_behind(pts, gate.last_emitted_pts) {
                    gate.open = true;
                    gate.last_emitted_pts = pts;
                    true
                } else {
                    false
                }
            }
            // Continuation fragments of an open PES, or PSI/NULL pids that never carry a
            // timestamp. Pids the gate knows about follow their verdict; unknown pids pass.
            None => self.pid_may_pass(pid),
        };
        if !passes {
            return false;
        }

        // The PES may pass while the PCR travelling with it is behind: that alone would make the
        // player jump its clock. Strip the obsolete PCR instead of losing the picture.
        if let Some(pcr) = Self::pcr_of(packet) {
            if self.pcr_is_behind(pid, pcr) {
                if !strip_pcr(packet) {
                    // Layout with other adaptation fields after PCR: leave it untouched rather
                    // than corrupt the packet. The PES itself is still worth delivering.
                    self.renumber_continuity(packet, pid, true);
                    return true;
                }
            } else {
                self.remember_pcr(pid, pcr);
            }
        }
        self.renumber_continuity(packet, pid, true);
        true
    }

    fn pid_may_pass(&self, pid: u16) -> bool {
        self.pids.get(&pid).is_none_or(|gate| gate.open)
    }

    /// PCR of this packet in 27 MHz ticks, when the adaptation field carries one.
    ///
    /// The adaptation field must actually be present: on a payload-only packet (control `0b01`)
    /// byte 4 is media data, and reading it as a length would turn picture bytes into a bogus
    /// clock that poisons every later comparison.
    fn pcr_of(packet: &[u8]) -> Option<u64> {
        if (packet[3] >> 4) & 0b10 == 0 {
            return None;
        }
        let adaptation_length = *packet.get(4)?;
        if adaptation_length == 0 || packet.get(5)? & 0x10 == 0 {
            return None;
        }
        let base = (u32::from(packet[6]) << 25)
            | (u32::from(packet[7]) << 17)
            | (u32::from(packet[8]) << 9)
            | (u32::from(packet[9]) << 1)
            | u32::from(packet[10] >> 7);
        let extension = (u32::from(packet[10] & 0x01) << 8) | u32::from(packet[11]);
        Some(u64::from(base) * 300 + u64::from(extension))
    }

    fn pcr_is_behind(&self, pid: u16, pcr: u64) -> bool {
        let Some(gate) = self.pids.get(&pid) else {
            return false;
        };
        let Some(last) = gate.last_emitted_pcr else {
            return false;
        };
        let forward = (pcr + PCR_MODULUS - last % PCR_MODULUS) % PCR_MODULUS;
        forward > PCR_HALF_RANGE && (PCR_MODULUS - forward) > PCR_BACKWARD_TOLERANCE
    }

    fn remember_pcr(&mut self, pid: u16, pcr: u64) {
        self.pids.entry(pid).or_insert_with(|| PidGate { last_emitted_pts: 0, open: true, last_emitted_pcr: None }).last_emitted_pcr = Some(pcr);
    }

    /// Rewrites the continuity counter so the delivered stream has no gaps.
    ///
    /// A packet without payload keeps the previous counter value, as ISO 13818-1 requires; a
    /// packet with payload advances it. The first packet of a pid adopts the provider's value so
    /// the stream does not start with a false break.
    fn renumber_continuity(&mut self, packet: &mut [u8], pid: u16, has_payload: bool) {
        if pid == NULL_PID {
            return;
        }
        let current = packet[3] & 0x0F;
        let next = match (has_payload, self.continuity.get(&pid)) {
            (true, Some(last)) => (last + 1) & 0x0F,
            (true, None) => current,
            (false, Some(&last)) => last,
            (false, None) => current,
        };
        packet[3] = (packet[3] & 0xF0) | next;
        self.continuity.insert(pid, next);
    }
}

/// Removes the PCR from an adaptation field that carries nothing else, turning the six PCR bytes
/// into stuffing. Returns false when the adaptation field has other optional fields after the PCR,
/// where clearing the flag would shift them.
fn strip_pcr(packet: &mut [u8]) -> bool {
    if packet.get(4).copied() != Some(7) || packet.get(5).copied() != Some(0x10) {
        return false;
    }
    packet[5] = 0x00;
    packet[6..12].fill(0xFF);
    true
}

/// Extracts the presentation timestamp of a PES packet that starts at `payload_start`.
fn parse_pes_pts(packet: &[u8], payload_start: usize) -> Option<u64> {
    let start = packet.get(payload_start..payload_start + 6)?;
    if start[0] != 0x00 || start[1] != 0x00 || start[2] != 0x01 {
        return None;
    }
    let stream_id = start[3];
    let is_media = matches!(stream_id, 0xC0..=0xDF | 0xE0..=0xEF);
    if !is_media {
        return None;
    }
    let header_data_length = *packet.get(payload_start + 8)?;
    if header_data_length < 5 {
        return None;
    }
    let flags = *packet.get(payload_start + 7)? >> 6;
    if flags == 0 {
        return None;
    }
    let t = packet.get(payload_start + 9..payload_start + 14)?;
    Some(
        (((t[0] >> 1) & 0x07) as u64) << 30
            | (((t[1] as u64) << 7 | (t[2] >> 1) as u64)) << 15
            | ((t[3] as u64) << 7 | (t[4] >> 1) as u64),
    )
}

/// Whether `pts` sits meaningfully behind `last` on the wrapping 33-bit clock.
fn pts_is_behind(pts: u64, last: u64) -> bool {
    let forward = (pts + PTS_MODULUS - last % PTS_MODULUS) % PTS_MODULUS;
    forward > PTS_HALF_RANGE && (PTS_MODULUS - forward) > REOPEN_BACKWARD_TOLERANCE_TICKS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal transport packet carrying a PES with the given presentation timestamp.
    fn packet(pid: u16, pts: u64) -> Vec<u8> {
        let mut packet = vec![0u8; TS_PACKET_SIZE];
        packet[0] = SYNC_BYTE;
        packet[1] = 0x40 | ((pid >> 8) & 0x1F) as u8; // payload unit start
        packet[2] = (pid & 0xFF) as u8;
        packet[3] = 0x10; // payload only
        let pts33 = pts % PTS_MODULUS;
        packet[4..10].copy_from_slice(&[
            0x00,
            0x00,
            0x01,
            0xE0,
            0x00,
            0x00, // PES: video stream id, no length (multipart)
        ]);
        packet[10] = 0x80; // flags marker '10'
        packet[11] = 0x80; // PTS_DTS flags: PTS only
        packet[12] = 5; // header data length
        packet[13] = 0x21 | (((pts33 >> 30) & 0x07) as u8) << 1;
        packet[14] = ((pts33 >> 22) & 0xFF) as u8;
        packet[15] = 0x01 | ((((pts33 >> 15) & 0x7F) as u8) << 1);
        packet[16] = ((pts33 >> 7) & 0xFF) as u8;
        packet[17] = 0x01 | ((pts33 & 0x7F) as u8) << 1;
        packet
    }

    /// Adaptation-field-only packet carrying a PCR, the shape a live video pid uses.
    fn pcr_packet(pid: u16, pcr: u64) -> Vec<u8> {
        let mut packet = vec![0xFFu8; TS_PACKET_SIZE];
        packet[0] = SYNC_BYTE;
        packet[1] = ((pid >> 8) & 0x1F) as u8;
        packet[2] = (pid & 0xFF) as u8;
        packet[3] = 0x20; // adaptation field only
        packet[4] = 183; // fill the packet
        packet[5] = 0x10; // PCR flag
        let base = (pcr / 300) % (1 << 33);
        let extension = pcr % 300;
        packet[6] = (base >> 25) as u8;
        packet[7] = (base >> 17) as u8;
        packet[8] = (base >> 9) as u8;
        packet[9] = (base >> 1) as u8;
        packet[10] = ((base & 0x01) as u8) << 7 | 0x7E | ((extension >> 8) as u8 & 0x01);
        packet[11] = (extension & 0xFF) as u8;
        packet
    }

    /// Packet carrying both a PES start and a PCR, as real video pids frequently do.
    fn pes_with_pcr(pid: u16, pts: u64, pcr: u64) -> Vec<u8> {
        let mut packet = packet(pid, pts);
        let pes: Vec<u8> = packet[4..].to_vec();
        packet[3] = 0x30; // adaptation field and payload
        packet[4] = 7;
        packet[5] = 0x10;
        let base = (pcr / 300) % (1 << 33);
        let extension = pcr % 300;
        packet[6] = (base >> 25) as u8;
        packet[7] = (base >> 17) as u8;
        packet[8] = (base >> 9) as u8;
        packet[9] = (base >> 1) as u8;
        packet[10] = ((base & 0x01) as u8) << 7 | 0x7E | ((extension >> 8) as u8 & 0x01);
        packet[11] = (extension & 0xFF) as u8;
        packet[12..].copy_from_slice(&pes[..TS_PACKET_SIZE - 12]);
        packet
    }

    fn pts_seconds(seconds: u64) -> u64 {
        seconds * 90_000
    }

    fn pcr_seconds(seconds: u64) -> u64 {
        seconds * 27_000_000
    }

    fn emitted(out: &[u8]) -> Vec<Vec<u8>> {
        out.chunks(TS_PACKET_SIZE).map(<[u8]>::to_vec).collect()
    }

    #[test]
    fn overlapped_prefix_is_dropped_and_continuation_passes() {
        let mut gate = MpegTsPtsGate::new();
        let mut out = Vec::new();

        // Episode 1 delivers up to 60 s.
        gate.push_chunk(&packet(256, pts_seconds(50)), &mut out);
        gate.push_chunk(&packet(256, pts_seconds(60)), &mut out);
        assert_eq!(out.len(), 2 * TS_PACKET_SIZE, "the first episode passes through");

        // Episode 2 replays from 50 s: the 50 s packet must be dropped, 60 s onwards passes.
        gate.begin_new_episode();
        out.clear();
        gate.push_chunk(&packet(256, pts_seconds(50)), &mut out);
        assert!(out.is_empty(), "a packet the viewer already received is dropped");
        gate.push_chunk(&packet(256, pts_seconds(60)), &mut out);
        assert_eq!(out.len(), TS_PACKET_SIZE, "the episode resumes at the last emitted timestamp");
    }

    #[test]
    fn a_small_backward_jitter_still_opens_the_gate() {
        let mut gate = MpegTsPtsGate::new();
        let mut out = Vec::new();
        gate.push_chunk(&packet(256, pts_seconds(100)), &mut out);

        gate.begin_new_episode();
        out.clear();
        gate.push_chunk(&packet(256, pts_seconds(100) - 4_500), &mut out);
        assert_eq!(out.len(), TS_PACKET_SIZE, "50 ms of encoder jitter is not an overlap");
    }

    #[test]
    fn audio_and_video_gate_independently() {
        let mut gate = MpegTsPtsGate::new();
        let mut out = Vec::new();
        gate.push_chunk(&packet(256, pts_seconds(30)), &mut out);
        gate.push_chunk(&packet(257, pts_seconds(30)), &mut out);

        gate.begin_new_episode();
        out.clear();
        // Video still overlapped, audio already past: only audio may pass.
        gate.push_chunk(&packet(256, pts_seconds(20)), &mut out);
        gate.push_chunk(&packet(257, pts_seconds(31)), &mut out);
        assert!(out.windows(TS_PACKET_SIZE).any(|window| window[2] == (257 & 0xFF) as u8), "audio continues");
        assert!(!out.windows(TS_PACKET_SIZE).any(|window| window[2] == (256 & 0xFF) as u8), "video stays held");
    }

    #[test]
    fn a_mid_body_rewind_re_arms_the_gate() {
        let mut gate = MpegTsPtsGate::new();
        let mut out = Vec::new();
        gate.push_chunk(&packet(256, pts_seconds(40)), &mut out);
        gate.push_chunk(&packet(256, pts_seconds(50)), &mut out);
        assert_eq!(out.len(), 2 * TS_PACKET_SIZE);

        // The provider rewinds inside the same body: the replay is dropped and delivery resumes
        // only once the timestamps are back at the last delivered point.
        out.clear();
        gate.push_chunk(&packet(256, pts_seconds(40)), &mut out);
        gate.push_chunk(&packet(256, pts_seconds(45)), &mut out);
        assert!(out.is_empty(), "a mid-body rewind is treated as replay");
        gate.push_chunk(&packet(256, pts_seconds(50)), &mut out);
        assert_eq!(out.len(), TS_PACKET_SIZE, "delivery resumes at the last delivered timestamp");
    }

    #[test]
    fn a_stream_without_transport_sync_never_locks_the_gate() {
        let mut gate = MpegTsPtsGate::new();
        let mut out = Vec::new();
        gate.push_chunk(b"first-episode", &mut out);
        gate.begin_new_episode();
        gate.push_chunk(b"second-episode", &mut out);
        assert_eq!(out, b"first-episodesecond-episode", "non-TS bytes pass through untouched");
    }

    #[test]
    fn partial_packets_are_reassembled_across_chunks() {
        let mut gate = MpegTsPtsGate::new();
        let packet = packet(256, pts_seconds(1));
        let mut out = Vec::new();
        gate.push_chunk(&packet[..100], &mut out);
        assert!(out.is_empty(), "an incomplete packet is held, never emitted early");
        gate.push_chunk(&packet[100..], &mut out);
        assert_eq!(out.len(), TS_PACKET_SIZE, "the packet is emitted once complete");
    }

    #[test]
    fn a_pcr_behind_the_delivered_one_is_never_emitted() {
        let mut gate = MpegTsPtsGate::new();
        let mut out = Vec::new();

        gate.push_chunk(&pcr_packet(256, pcr_seconds(100)), &mut out);
        assert_eq!(out.len(), TS_PACKET_SIZE, "the first PCR passes");

        // The reopened episode replays from slightly behind: its PCR would move the player's
        // clock backwards, so it must not be delivered.
        out.clear();
        gate.push_chunk(&pcr_packet(256, pcr_seconds(100) - 27_000 * 67), &mut out);
        assert!(out.is_empty(), "a PCR behind the delivered one is withheld");

        gate.push_chunk(&pcr_packet(256, pcr_seconds(101)), &mut out);
        assert_eq!(out.len(), TS_PACKET_SIZE, "the clock resumes once it is ahead again");
    }

    #[test]
    fn an_obsolete_pcr_riding_a_live_pes_is_stripped_not_dropped() {
        let mut gate = MpegTsPtsGate::new();
        let mut out = Vec::new();
        gate.push_chunk(&pes_with_pcr(256, pts_seconds(50), pcr_seconds(50)), &mut out);
        assert_eq!(out.len(), TS_PACKET_SIZE);

        // Picture is new, clock is behind: keep the picture and remove the stale clock.
        out.clear();
        gate.push_chunk(&pes_with_pcr(256, pts_seconds(51), pcr_seconds(50) - 27_000 * 67), &mut out);
        let packets = emitted(&out);
        assert_eq!(packets.len(), 1, "the picture is still delivered");
        assert_eq!(packets[0][5], 0x00, "the PCR flag is cleared");
        assert!(packets[0][6..12].iter().all(|&b| b == 0xFF), "the PCR bytes became stuffing");
        assert_eq!(MpegTsPtsGate::pcr_of(&packets[0]), None, "no stale clock reaches the player");
    }

    #[test]
    fn deleting_packets_never_breaks_the_continuity_counter() {
        let mut gate = MpegTsPtsGate::new();
        let mut out = Vec::new();
        gate.push_chunk(&packet(256, pts_seconds(50)), &mut out);
        gate.push_chunk(&packet(256, pts_seconds(60)), &mut out);

        // Episode 2 restarts the provider's own counter sequence; the replay in between is dropped.
        gate.begin_new_episode();
        out.clear();
        let mut replayed = packet(256, pts_seconds(50));
        replayed[3] = 0x10 | 0x0C; // the provider starts this episode at a different counter
        gate.push_chunk(&replayed, &mut out);
        let mut resumed = packet(256, pts_seconds(60));
        resumed[3] = 0x10 | 0x0D;
        gate.push_chunk(&resumed, &mut out);

        let packets = emitted(&out);
        assert_eq!(packets.len(), 1, "only the non-overlapped packet is delivered");
        let first_episode_last = 1u8; // two packets starting at 0
        assert_eq!(
            packets[0][3] & 0x0F,
            (first_episode_last + 1) & 0x0F,
            "the emitted counter continues the delivered sequence, not the provider's"
        );
    }

    #[test]
    fn a_packet_without_an_adaptation_field_never_yields_a_pcr() {
        // Payload-only packets put media bytes where a PCR would live. Reading them as a clock
        // poisoned `last_emitted_pcr`, after which every real PCR looked stale and was stripped.
        let packet = packet(256, pts_seconds(10));
        assert_eq!(packet[3] >> 4, 0b01, "this fixture carries payload only");
        assert_eq!(MpegTsPtsGate::pcr_of(&packet), None, "no adaptation field means no PCR");
    }

    #[test]
    fn a_packet_without_payload_keeps_the_counter() {
        let mut gate = MpegTsPtsGate::new();
        let mut out = Vec::new();
        gate.push_chunk(&packet(256, pts_seconds(10)), &mut out);
        out.clear();
        gate.push_chunk(&pcr_packet(256, pcr_seconds(10)), &mut out);
        let packets = emitted(&out);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0][3] & 0x0F, 0, "an adaptation-only packet does not advance the counter");
    }
}
