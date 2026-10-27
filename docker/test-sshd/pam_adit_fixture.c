/* Test-only deterministic second factor. Never install this on a real host.
 * pam_unix authenticates the account password first; every new SSH connection
 * then asks this independent challenge. This is not a TOTP verifier. */
#include <security/pam_modules.h>
#include <security/pam_appl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

PAM_EXTERN int pam_sm_authenticate(pam_handle_t *pamh, int flags,
                                  int argc, const char **argv)
{
    (void)flags; (void)argc; (void)argv;
    const void *item = NULL;
    if (pam_get_item(pamh, PAM_CONV, &item) != PAM_SUCCESS || item == NULL)
        return PAM_CONV_ERR;
    const struct pam_conv *conv = item;
    struct pam_message message = { PAM_PROMPT_ECHO_OFF, "ADIT fixture verification code: " };
    const struct pam_message *messages[] = { &message };
    struct pam_response *answer = NULL;
    int result = conv->conv(1, messages, &answer, conv->appdata_ptr);
    if (result != PAM_SUCCESS || answer == NULL)
        return PAM_CONV_ERR;
    char expected[128] = {0};
    FILE *file = fopen("/etc/adit-fixture-code", "r");
    if (file == NULL || fgets(expected, sizeof expected, file) == NULL) {
        result = PAM_AUTH_ERR;
    } else {
        expected[strcspn(expected, "\r\n")] = '\0';
        result = answer[0].resp != NULL && strcmp(answer[0].resp, expected) == 0
            ? PAM_SUCCESS : PAM_AUTH_ERR;
    }
    if (file != NULL) fclose(file);
    if (answer[0].resp != NULL) {
        memset(answer[0].resp, 0, strlen(answer[0].resp));
        free(answer[0].resp);
    }
    free(answer);
    return result;
}

PAM_EXTERN int pam_sm_setcred(pam_handle_t *pamh, int flags,
                             int argc, const char **argv)
{
    (void)pamh; (void)flags; (void)argc; (void)argv;
    return PAM_SUCCESS;
}
