Parts of the packet formats and timing behavior in this directory were ported
from DoubleTake, commit 962b47f609f88098d809e0ef680280539bb685fd,
https://github.com/jmh-zero/doubletake, licensed LGPL-3.0-or-later. These
derived portions remain available under LGPL-3.0-or-later. The upstream license
text is reproduced in LICENSE-LGPL-3.0 in this directory. This module is not yet
a complete AirPlay implementation: HAP PIN setup, authenticated pair-verify,
control-channel framing and encrypted credential files are implemented, but
FairPlay SAP, keyring integration and transient/raw pairing are not. GStreamer
capture, a media loop and an NTP/PTP HAP session are integrated experimentally.
When FPSAP is not advertised, session keys are derived from pair-verify;
FPSAP-capable receivers still require FairPlay material generated elsewhere.
The no-FPSAP PIN/PTP path has been exercised against the DoubleTake test receiver
but reception by a physical TV and end-to-end audio/video quality have not been
verified. The implemented
PIN path uses num-bigint's variable-time arithmetic; it has not received a security
audit. Treat this backend as experimental.
