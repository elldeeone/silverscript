#ifndef KASPA_STATE_BRIDGE_H
#define KASPA_STATE_BRIDGE_H

#include "d_ticcmd.h"
#include "doomtype.h"

void KaspaStateBridge_AfterTic(int committed_tic, const ticcmd_t *cmds, const boolean *ingame, int max_players);

#endif
