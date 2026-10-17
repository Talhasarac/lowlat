/* Drives a library that carries the virtual displays alone, the way an
 * application built against it does.
 *
 * Both other halves hidden, so a call into either would not compile; linked
 * through the import library rather than opened by path, so a name the third
 * half lost is an unresolved symbol here. The driver may be installed where
 * this runs or not, and this account admitted or not: any answer from the
 * third half's own range says the call reached it.
 */

#define LOWLAT_NO_HOST
#define LOWLAT_NO_CLIENT
#include "lowlat.h"

#include <stdio.h>

int main(void)
{
    uint32_t version = lowlat_abi_version();
    if ((version >> 16) != LOWLAT_ABI_MAJOR || (version & 0xffff) != LOWLAT_ABI_MINOR) {
        fprintf(stderr, "vdd-only: the library reports version %u.%u, the header says %u.%u\n",
                version >> 16, version & 0xffff,
                (unsigned) LOWLAT_ABI_MAJOR, (unsigned) LOWLAT_ABI_MINOR);
        return 1;
    }
    if (lowlat_features() != LOWLAT_FEATURE_VDD) {
        fprintf(stderr, "vdd-only: the library carries features 0x%x, not the virtual displays alone\n",
                (unsigned) lowlat_features());
        return 1;
    }

    lowlat_vdd *vdd = NULL;
    lowlat_status opened = lowlat_vdd_open(&vdd);
    if (opened == LOWLAT_OK) {
        uint32_t count = 0;
        lowlat_status listed = lowlat_vdd_list(vdd, NULL, &count);
        lowlat_vdd_close(vdd);
        if (listed != LOWLAT_OK || count > LOWLAT_VDD_SLOTS) {
            fprintf(stderr, "vdd-only: the driver's list answered %d with %u slots\n",
                    (int) listed, count);
            return 1;
        }
    } else if ((int) opened > -600 || (int) opened <= -700) {
        fprintf(stderr, "vdd-only: lowlat_vdd_open answered %d (%s), outside the third half's range\n",
                (int) opened, lowlat_status_string(opened));
        return 1;
    }

    printf("vdd-only: version %u.%u, the driver: %s\n", version >> 16, version & 0xffff,
           opened == LOWLAT_OK ? "opened" : lowlat_status_string(opened));
    return 0;
}
