#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <libusb-1.0/libusb.h>

#define VID             0x0403
#define PID             0x6001
#define ENDPOINT        0x02

#define USB_TIMEOUT_MS  1000
#define MAX_SERIAL_LEN  256

/*
 * FT245R bitbang commands used by your existing code.
 */
#define BMREQ_ENABLE    0x40
#define REQ_ENABLE      0x0B
#define VALUE_ENABLE    0x01FF
#define INDEX_ENABLE    0x01

#define BMREQ_READ      0xC0
#define REQ_READ        0x0C
#define VALUE_READ      0x0000
#define INDEX_READ      0x01

#define MAX_RELAYS      8


static void usage(const char *prog)
{
    printf(
        "Usage:\n"
        "  %s --serial <serial> --relay <1-8|all> --state <on|off>\n"
        "  %s --serial <serial> --status\n"
        "\n"
        "Examples:\n"
        "  %s --serial A1234567 --relay 1 --state on\n"
        "  %s --serial A1234567 --relay 4 --state off\n"
        "  %s --serial A1234567 --relay all --state on\n"
        "  %s --serial A1234567 --relay all --state off\n"
        "  %s --serial A1234567 --status\n"
        "\n"
        "Relay numbering is 1-8.\n",
        prog, prog,
        prog, prog, prog, prog, prog
    );
}


/*
 * Find FT245R relay device by USB serial number.
 */
static libusb_device_handle *
find_device(libusb_context *ctx, const char *serial)
{
    libusb_device **devs = NULL;
    libusb_device_handle *handle = NULL;

    ssize_t count = libusb_get_device_list(ctx, &devs);

    if (count < 0) {
        fprintf(stderr, "libusb_get_device_list failed: %s\n",
                libusb_error_name((int)count));
        return NULL;
    }

    for (ssize_t i = 0; i < count; i++) {

        libusb_device *dev = devs[i];
        struct libusb_device_descriptor desc;

        int ret = libusb_get_device_descriptor(dev, &desc);
        if (ret != 0)
            continue;

        if (desc.idVendor != VID || desc.idProduct != PID)
            continue;

        libusb_device_handle *tmp = NULL;

        ret = libusb_open(dev, &tmp);
        if (ret != 0) {
            fprintf(stderr,
                    "Could not open USB device: %s\n",
                    libusb_error_name(ret));
            continue;
        }

        if (!desc.iSerialNumber) {
            libusb_close(tmp);
            continue;
        }

        unsigned char device_serial[MAX_SERIAL_LEN];

        ret = libusb_get_string_descriptor_ascii(
            tmp,
            desc.iSerialNumber,
            device_serial,
            sizeof(device_serial)
        );

        if (ret > 0) {

            if (strcmp((char *)device_serial, serial) == 0) {
                handle = tmp;
                break;
            }
        }

        libusb_close(tmp);
    }

    libusb_free_device_list(devs, 1);

    return handle;
}


/*
 * Enable FT245R bitbang mode.
 */
static int enable_bitbang(libusb_device_handle *handle)
{
    int ret;

    if (libusb_kernel_driver_active(handle, 0) == 1) {

        ret = libusb_detach_kernel_driver(handle, 0);

        if (ret != 0) {
            fprintf(stderr,
                    "Warning: could not detach kernel driver: %s\n",
                    libusb_error_name(ret));
        }
    }

    ret = libusb_claim_interface(handle, 0);

    if (ret != 0) {
        fprintf(stderr,
                "Could not claim interface 0: %s\n",
                libusb_error_name(ret));
        return ret;
    }

    ret = libusb_control_transfer(
        handle,
        BMREQ_ENABLE,
        REQ_ENABLE,
        VALUE_ENABLE,
        INDEX_ENABLE,
        NULL,
        0,
        USB_TIMEOUT_MS
    );

    if (ret < 0) {
        fprintf(stderr,
                "Failed to enable bitbang mode: %s\n",
                libusb_error_name(ret));

        libusb_release_interface(handle, 0);
        return ret;
    }

    return 0;
}


/*
 * Read the current 8-bit relay state.
 *
 * Bit 0 = relay 1
 * Bit 1 = relay 2
 * ...
 * Bit 7 = relay 8
 */
static int read_relay_state(
    libusb_device_handle *handle,
    unsigned char *state)
{
    unsigned char buf[2] = {0};

    int ret = libusb_control_transfer(
        handle,
        BMREQ_READ,
        REQ_READ,
        VALUE_READ,
        INDEX_READ,
        buf,
        sizeof(buf),
        USB_TIMEOUT_MS
    );

    if (ret < 0) {
        fprintf(stderr,
                "Failed to read relay state: %s\n",
                libusb_error_name(ret));
        return ret;
    }

    if (ret < 1) {
        fprintf(stderr,
                "Failed to read relay state: only %d bytes received\n",
                ret);
        return -1;
    }

    *state = buf[0];

    return 0;
}


/*
 * Write complete 8-bit relay state.
 */
static int write_relay_state(
    libusb_device_handle *handle,
    unsigned char state)
{
    int transferred = 0;

    int ret = libusb_bulk_transfer(
        handle,
        ENDPOINT,
        &state,
        1,
        &transferred,
        USB_TIMEOUT_MS
    );

    if (ret != 0) {
        fprintf(stderr,
                "Failed to write relay state: %s\n",
                libusb_error_name(ret));
        return ret;
    }

    if (transferred != 1) {
        fprintf(stderr,
                "Failed to write relay state: transferred=%d\n",
                transferred);
        return -1;
    }

    return 0;
}


static void print_state(unsigned char state)
{
    printf("Relay state: 0x%02X\n", state);

    for (int i = 0; i < MAX_RELAYS; i++) {
        printf(
            "  Relay %d: %s\n",
            i + 1,
            (state & (1 << i)) ? "ON" : "OFF"
        );
    }
}


static int parse_state(const char *str)
{
    if (strcasecmp(str, "on") == 0)
        return 1;

    if (strcasecmp(str, "off") == 0)
        return 0;

    return -1;
}


int main(int argc, char **argv)
{
    const char *serial = NULL;
    const char *relay_arg = NULL;
    const char *state_arg = NULL;

    int status_only = 0;

    /*
     * Parse command line.
     */
    for (int i = 1; i < argc; i++) {

        if (strcmp(argv[i], "--serial") == 0) {

            if (++i >= argc) {
                fprintf(stderr, "--serial requires an argument\n");
                return EXIT_FAILURE;
            }

            serial = argv[i];
        }

        else if (strcmp(argv[i], "--relay") == 0) {

            if (++i >= argc) {
                fprintf(stderr, "--relay requires an argument\n");
                return EXIT_FAILURE;
            }

            relay_arg = argv[i];
        }

        else if (strcmp(argv[i], "--state") == 0) {

            if (++i >= argc) {
                fprintf(stderr, "--state requires an argument\n");
                return EXIT_FAILURE;
            }

            state_arg = argv[i];
        }

        else if (strcmp(argv[i], "--status") == 0) {
            status_only = 1;
        }

        else if (
            strcmp(argv[i], "--help") == 0 ||
            strcmp(argv[i], "-h") == 0
        ) {
            usage(argv[0]);
            return EXIT_SUCCESS;
        }

        else {
            fprintf(stderr, "Unknown argument: %s\n", argv[i]);
            usage(argv[0]);
            return EXIT_FAILURE;
        }
    }

    /*
     * Serial number is always required.
     */
    if (!serial) {
        fprintf(stderr, "--serial is required\n");
        usage(argv[0]);
        return EXIT_FAILURE;
    }

    /*
     * --status does not need relay/state arguments.
     */
    if (status_only) {

        if (relay_arg || state_arg) {
            fprintf(stderr,
                    "--status cannot be combined with --relay or --state\n");
            return EXIT_FAILURE;
        }
    }
    else {

        if (!relay_arg || !state_arg) {
            fprintf(stderr,
                    "--relay and --state are required\n");
            usage(argv[0]);
            return EXIT_FAILURE;
        }
    }

    /*
     * Parse ON/OFF.
     */
    int state_value = -1;

    if (!status_only) {
        state_value = parse_state(state_arg);

        if (state_value < 0) {
            fprintf(stderr,
                    "Invalid state '%s'. Use 'on' or 'off'.\n",
                    state_arg);
            return EXIT_FAILURE;
        }
    }

    /*
     * Parse relay.
     *
     * relay = 1..8
     * relay = all
     */
    int relay = -1;

    if (!status_only) {

        if (strcasecmp(relay_arg, "all") == 0) {
            relay = 0;
        }
        else {
            char *endptr = NULL;

            long value = strtol(relay_arg, &endptr, 10);

            if (*relay_arg == '\0' ||
                *endptr != '\0' ||
                value < 1 ||
                value > MAX_RELAYS) {

                fprintf(stderr,
                        "Invalid relay '%s'. Use 1-8 or all.\n",
                        relay_arg);
                return EXIT_FAILURE;
            }

            relay = (int)value;
        }
    }

    /*
     * Initialize libusb.
     */
    libusb_context *ctx = NULL;

    int ret = libusb_init(&ctx);

    if (ret != 0) {
        fprintf(stderr,
                "libusb_init failed: %s\n",
                libusb_error_name(ret));
        return EXIT_FAILURE;
    }

    /*
     * Find requested relay controller.
     */
    libusb_device_handle *handle =
        find_device(ctx, serial);

    if (!handle) {
        fprintf(stderr,
                "Relay controller with serial '%s' not found\n",
                serial);

        libusb_exit(ctx);
        return EXIT_FAILURE;
    }

    printf("Found relay controller: %s\n", serial);

    /*
     * Enable FTDI bitbang mode.
     */
    ret = enable_bitbang(handle);

    if (ret != 0) {
        libusb_close(handle);
        libusb_exit(ctx);
        return EXIT_FAILURE;
    }

    /*
     * Read current state.
     */
    unsigned char current_state = 0;

    ret = read_relay_state(handle, &current_state);

    if (ret != 0) {
        libusb_release_interface(handle, 0);
        libusb_close(handle);
        libusb_exit(ctx);
        return EXIT_FAILURE;
    }

    /*
     * Status only.
     */
    if (status_only) {

        print_state(current_state);

        libusb_release_interface(handle, 0);
        libusb_close(handle);
        libusb_exit(ctx);

        return EXIT_SUCCESS;
    }

    /*
     * Modify relay state.
     */
    unsigned char new_state = current_state;

    if (relay == 0) {

        /*
         * all
         */
        if (state_value)
            new_state = 0xFF;
        else
            new_state = 0x00;
    }
    else {

        /*
         * Individual relay.
         *
         * User relay 1 => bit 0
         * User relay 2 => bit 1
         * ...
         */
        unsigned char bit = (unsigned char)(1 << (relay - 1));

        if (state_value)
            new_state |= bit;
        else
            new_state &= (unsigned char)~bit;
    }

    printf(
        "Current state: 0x%02X\n"
        "New state:     0x%02X\n",
        current_state,
        new_state
    );

    /*
     * Nothing to do.
     */
    if (current_state == new_state) {
        printf("Relay state already matches requested state.\n");

        libusb_release_interface(handle, 0);
        libusb_close(handle);
        libusb_exit(ctx);

        return EXIT_SUCCESS;
    }

    /*
     * Write new state.
     */
    ret = write_relay_state(handle, new_state);

    if (ret != 0) {
        fprintf(stderr,
                "Failed to update relay state\n");

        libusb_release_interface(handle, 0);
        libusb_close(handle);
        libusb_exit(ctx);

        return EXIT_FAILURE;
    }

    printf("Relay state updated successfully.\n");

    libusb_release_interface(handle, 0);
    libusb_close(handle);
    libusb_exit(ctx);

    return EXIT_SUCCESS;
}

