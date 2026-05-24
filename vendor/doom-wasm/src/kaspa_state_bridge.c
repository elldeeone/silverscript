#include <stdio.h>

#ifdef __EMSCRIPTEN__
#include <emscripten.h>
#endif

#include "kaspa_state_bridge.h"
#include "doomstat.h"
#include "p_local.h"
#include "p_mobj.h"
#include "p_spec.h"
#include "r_state.h"

extern int rndindex;
extern int prndindex;

#define KASPA_STATE_BYTES 96
#define KASPA_TICCMD_OFFSET 88
#define FNV64_OFFSET 1469598103934665603ULL
#define FNV64_PRIME 1099511628211ULL

static void PackTiccmd(const ticcmd_t *cmd, unsigned char out[8])
{
    out[0] = (unsigned char)cmd->forwardmove;
    out[1] = (unsigned char)cmd->sidemove;
    out[2] = (unsigned char)(cmd->angleturn & 0xff);
    out[3] = (unsigned char)((cmd->angleturn >> 8) & 0xff);
    out[4] = cmd->buttons;
    out[5] = cmd->consistancy;
    out[6] = cmd->buttons2;
    out[7] = cmd->lookfly;
}

static void HashU32(unsigned long long *hash, unsigned int value)
{
    int i;

    for (i = 0; i < 4; ++i)
    {
        *hash ^= (unsigned char)((value >> (i * 8)) & 0xff);
        *hash *= FNV64_PRIME;
    }
}

static void HashI32(unsigned long long *hash, int value)
{
    HashU32(hash, (unsigned int)value);
}

static void HashBytes(unsigned long long *hash, const unsigned char *bytes, int len)
{
    int i;

    for (i = 0; i < len; ++i)
    {
        *hash ^= bytes[i];
        *hash *= FNV64_PRIME;
    }
}

static void WriteU32(unsigned char *out, int offset, unsigned int value)
{
    out[offset] = (unsigned char)(value & 0xff);
    out[offset + 1] = (unsigned char)((value >> 8) & 0xff);
    out[offset + 2] = (unsigned char)((value >> 16) & 0xff);
    out[offset + 3] = (unsigned char)((value >> 24) & 0xff);
}

static void WriteU64(unsigned char *out, int offset, unsigned long long value)
{
    int i;

    for (i = 0; i < 8; ++i)
    {
        out[offset + i] = (unsigned char)((value >> (i * 8)) & 0xff);
    }
}

static void PrintHex(const unsigned char *bytes, int len)
{
    int i;

    for (i = 0; i < len; ++i)
    {
        printf("%02x", bytes[i]);
    }
}

static unsigned long long HashPlayers(const boolean *ingame, int max_players, int *live_players)
{
    int i;
    int j;
    unsigned long long hash = FNV64_OFFSET;

    *live_players = 0;
    for (i = 0; i < max_players && i < MAXPLAYERS; ++i)
    {
        player_t *player;

        if (!ingame[i])
        {
            continue;
        }

        ++*live_players;
        player = &players[i];
        HashI32(&hash, i);
        HashI32(&hash, player->playerstate);
        HashBytes(&hash, (const unsigned char *)&player->cmd, sizeof(player->cmd));
        HashI32(&hash, player->viewz);
        HashI32(&hash, player->viewheight);
        HashI32(&hash, player->deltaviewheight);
        HashI32(&hash, player->bob);
        HashI32(&hash, player->health);
        HashI32(&hash, player->armorpoints);
        HashI32(&hash, player->armortype);
        HashI32(&hash, player->readyweapon);
        HashI32(&hash, player->pendingweapon);
        HashI32(&hash, player->attackdown);
        HashI32(&hash, player->usedown);
        HashI32(&hash, player->cheats);
        HashI32(&hash, player->refire);
        HashI32(&hash, player->killcount);
        HashI32(&hash, player->itemcount);
        HashI32(&hash, player->secretcount);
        HashI32(&hash, player->damagecount);
        HashI32(&hash, player->bonuscount);
        HashI32(&hash, player->extralight);
        HashI32(&hash, player->fixedcolormap);
        HashI32(&hash, player->colormap);
        HashI32(&hash, player->didsecret);

        for (j = 0; j < NUMPOWERS; ++j)
        {
            HashI32(&hash, player->powers[j]);
        }
        for (j = 0; j < NUMCARDS; ++j)
        {
            HashI32(&hash, player->cards[j]);
        }
        for (j = 0; j < NUMWEAPONS; ++j)
        {
            HashI32(&hash, player->weaponowned[j]);
        }
        for (j = 0; j < NUMAMMO; ++j)
        {
            HashI32(&hash, player->ammo[j]);
            HashI32(&hash, player->maxammo[j]);
        }
        for (j = 0; j < NUMPSPRITES; ++j)
        {
            HashI32(&hash, player->psprites[j].state == NULL ? -1 : (int)(player->psprites[j].state - states));
            HashI32(&hash, player->psprites[j].tics);
            HashI32(&hash, player->psprites[j].sx);
            HashI32(&hash, player->psprites[j].sy);
        }
    }

    return hash;
}

static unsigned long long HashMobjs(int *mobj_count)
{
    thinker_t *thinker;
    unsigned long long hash = FNV64_OFFSET;
    int guard = 0;

    *mobj_count = 0;
    for (thinker = thinkercap.next; thinker != &thinkercap && guard < 10000; thinker = thinker->next, ++guard)
    {
        mobj_t *mobj;

        if (thinker->function.acp1 != (actionf_p1)P_MobjThinker)
        {
            continue;
        }

        ++*mobj_count;
        mobj = (mobj_t *)thinker;
        HashI32(&hash, mobj->x);
        HashI32(&hash, mobj->y);
        HashI32(&hash, mobj->z);
        HashI32(&hash, mobj->angle);
        HashI32(&hash, mobj->sprite);
        HashI32(&hash, mobj->frame);
        HashI32(&hash, mobj->floorz);
        HashI32(&hash, mobj->ceilingz);
        HashI32(&hash, mobj->momx);
        HashI32(&hash, mobj->momy);
        HashI32(&hash, mobj->momz);
        HashI32(&hash, mobj->type);
        HashI32(&hash, mobj->tics);
        HashI32(&hash, mobj->state == NULL ? -1 : (int)(mobj->state - states));
        HashI32(&hash, mobj->flags);
        HashI32(&hash, mobj->health);
        HashI32(&hash, mobj->movedir);
        HashI32(&hash, mobj->movecount);
        HashI32(&hash, mobj->reactiontime);
        HashI32(&hash, mobj->threshold);
        HashI32(&hash, mobj->lastlook);
        HashI32(&hash, mobj->spawnpoint.x);
        HashI32(&hash, mobj->spawnpoint.y);
        HashI32(&hash, mobj->spawnpoint.angle);
        HashI32(&hash, mobj->spawnpoint.type);
        HashI32(&hash, mobj->spawnpoint.options);
    }

    return hash;
}

static int SectorIndex(const sector_t *sector)
{
    if (sector == NULL || sectors == NULL)
    {
        return -1;
    }

    return (int)(sector - sectors);
}

static unsigned long long HashWorld(void)
{
    int i;
    int j;
    unsigned long long hash = FNV64_OFFSET;

    HashI32(&hash, numsectors);
    for (i = 0; i < numsectors; ++i)
    {
        const sector_t *sec = &sectors[i];
        HashI32(&hash, sec->floorheight >> FRACBITS);
        HashI32(&hash, sec->ceilingheight >> FRACBITS);
        HashI32(&hash, sec->floorpic);
        HashI32(&hash, sec->ceilingpic);
        HashI32(&hash, sec->lightlevel);
        HashI32(&hash, sec->special);
        HashI32(&hash, sec->tag);
    }

    HashI32(&hash, numlines);
    for (i = 0; i < numlines; ++i)
    {
        const line_t *line = &lines[i];
        HashI32(&hash, line->flags);
        HashI32(&hash, line->special);
        HashI32(&hash, line->tag);
        for (j = 0; j < 2; ++j)
        {
            if (line->sidenum[j] == -1)
            {
                HashI32(&hash, -1);
                continue;
            }

            HashI32(&hash, line->sidenum[j]);
            HashI32(&hash, sides[line->sidenum[j]].textureoffset >> FRACBITS);
            HashI32(&hash, sides[line->sidenum[j]].rowoffset >> FRACBITS);
            HashI32(&hash, sides[line->sidenum[j]].toptexture);
            HashI32(&hash, sides[line->sidenum[j]].bottomtexture);
            HashI32(&hash, sides[line->sidenum[j]].midtexture);
        }
    }

    return hash;
}

static unsigned long long HashSpecialThinkers(int *special_count)
{
    int i;
    thinker_t *thinker;
    unsigned long long hash = FNV64_OFFSET;
    int guard = 0;

    *special_count = 0;
    for (thinker = thinkercap.next; thinker != &thinkercap && guard < 10000; thinker = thinker->next, ++guard)
    {
        if (thinker->function.acv == (actionf_v)NULL)
        {
            for (i = 0; i < MAXCEILINGS; ++i)
            {
                if (activeceilings[i] == (ceiling_t *)thinker)
                {
                    ++*special_count;
                    HashI32(&hash, 1);
                    HashI32(&hash, i);
                    break;
                }
            }
            continue;
        }

        if (thinker->function.acp1 == (actionf_p1)T_MoveCeiling)
        {
            ++*special_count;
            HashI32(&hash, 1);
        }
        else if (thinker->function.acp1 == (actionf_p1)T_VerticalDoor)
        {
            ++*special_count;
            HashI32(&hash, 2);
        }
        else if (thinker->function.acp1 == (actionf_p1)T_MoveFloor)
        {
            ++*special_count;
            HashI32(&hash, 3);
        }
        else if (thinker->function.acp1 == (actionf_p1)T_PlatRaise)
        {
            ++*special_count;
            HashI32(&hash, 4);
        }
        else if (thinker->function.acp1 == (actionf_p1)T_LightFlash)
        {
            ++*special_count;
            HashI32(&hash, 5);
        }
        else if (thinker->function.acp1 == (actionf_p1)T_StrobeFlash)
        {
            ++*special_count;
            HashI32(&hash, 6);
        }
        else if (thinker->function.acp1 == (actionf_p1)T_Glow)
        {
            ++*special_count;
            HashI32(&hash, 7);
        }
    }

    return hash;
}

static void PackCompactState(int committed_tic, int player, const unsigned char ticcmd[8], const boolean *ingame, int max_players, unsigned char out[KASPA_STATE_BYTES])
{
    int i;
    unsigned char mask = 0;
    int live_players = 0;
    int mobj_count = 0;
    unsigned long long player_hash;
    unsigned long long mobj_hash;
    unsigned long long world_hash;
    unsigned long long special_hash;
    int special_count = 0;

    for (i = 0; i < KASPA_STATE_BYTES; ++i)
    {
        out[i] = 0;
    }

    for (i = 0; i < max_players && i < 8; ++i)
    {
        if (ingame[i])
        {
            mask |= (unsigned char)(1 << i);
        }
    }
    player_hash = HashPlayers(ingame, max_players, &live_players);
    mobj_hash = HashMobjs(&mobj_count);
    world_hash = HashWorld();
    special_hash = HashSpecialThinkers(&special_count);
    HashBytes(&special_hash, ticcmd, 8);

    out[0] = 'K';
    out[1] = 'D';
    out[2] = 'S';
    out[3] = '4';
    WriteU32(out, 4, (unsigned int)committed_tic);
    WriteU32(out, 8, (unsigned int)leveltime);
    out[12] = (unsigned char)(prndindex & 0xff);
    out[13] = (unsigned char)(rndindex & 0xff);
    out[14] = (unsigned char)player;
    out[15] = mask;
    WriteU32(out, 16, (unsigned int)max_players);
    WriteU32(out, 20, (unsigned int)live_players);
    WriteU32(out, 24, (unsigned int)mobj_count);
    WriteU64(out, 28, player_hash);
    WriteU64(out, 36, mobj_hash);
    WriteU64(out, 44, world_hash);
    WriteU64(out, 52, special_hash);
    WriteU32(out, 60, (unsigned int)special_count);
    WriteU32(out, 64, (unsigned int)numsectors);
    WriteU32(out, 68, (unsigned int)numlines);
    WriteU32(out, 72, (unsigned int)numsides);
    WriteU32(out, 76, (unsigned int)totalkills);
    WriteU32(out, 80, (unsigned int)totalitems);
    WriteU32(out, 84, (unsigned int)totalsecret);
    for (i = 0; i < 8; ++i)
    {
        out[KASPA_TICCMD_OFFSET + i] = ticcmd[i];
    }
}

void KaspaStateBridge_AfterTic(int committed_tic, const ticcmd_t *cmds, const boolean *ingame, int max_players)
{
    unsigned char packed[8] = {0};
    unsigned char state[KASPA_STATE_BYTES] = {0};
    int player;
    int active_player = -1;

    for (player = 0; player < max_players; ++player)
    {
        if (ingame[player])
        {
            PackTiccmd(&cmds[player], packed);
            active_player = player;
            break;
        }
    }
    PackCompactState(committed_tic, active_player, packed, ingame, max_players, state);

    printf("kaspa-doom:tic:%d:%02x%02x%02x%02x%02x%02x%02x%02x:",
           committed_tic,
           packed[0],
           packed[1],
           packed[2],
           packed[3],
           packed[4],
           packed[5],
           packed[6],
           packed[7]);
    PrintHex(state, KASPA_STATE_BYTES);
    printf("\n");

#ifdef __EMSCRIPTEN__
    EM_ASM({
        if (typeof Module !== 'undefined' && typeof Module.kaspaDoomOnTic === 'function') {
            var ticcmd = Array.prototype.slice.call(HEAPU8.subarray($1, $1 + 8));
            var state = Array.prototype.slice.call(HEAPU8.subarray($2, $2 + $3));
            Module.kaspaDoomOnTic($0, ticcmd, state);
        }
    }, committed_tic, packed, state, KASPA_STATE_BYTES);
#endif
}
