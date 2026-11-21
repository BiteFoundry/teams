# Register a company application for Teams

Register a single-tenant desktop application in Microsoft Entra ID to associate Teams CLI sign-ins, consent and audit records with your organization. Outlook, OneNote and SharePoint use the same desktop registration procedure with their own Graph permissions.

## Set up the application

1. In the Microsoft Entra admin center, open **App registrations → New registration**. Choose **Accounts in this organizational directory only**, register the app, and copy its **Application (client) ID** and **Directory (tenant) ID** from Overview.
2. Under **Authentication**, add the **Mobile and desktop applications** platform and the redirect URI `http://localhost`. Teams uses authorization code with PKCE and opens a temporary loopback port. For headless sign-in with `--device-code`, enable **Allow public client flows** if your tenant permits device-code authentication.
3. Under **API permissions → Add a permission → Microsoft Graph → Delegated permissions**, add the scopes for the features you enable. For permissions requiring administrator consent, have an authorized tenant administrator grant consent. [Register a desktop application](https://learn.microsoft.com/en-us/entra/identity-platform/scenario-desktop-app-registration) · [Configure API permissions](https://learn.microsoft.com/en-us/entra/identity-platform/quickstart-configure-app-access-web-apis)

| Teams feature | Delegated permissions requested |
| --- | --- |
| Sign-in, token renewal and user lookup | `User.Read`, `offline_access`, `User.ReadBasic.All` |
| Read and send chats, edit your own chat messages | `Chat.ReadWrite` |
| List teams and channels | `Team.ReadBasic.All`, `Channel.ReadBasic.All` |
| Read channel history and send channel posts | `ChannelMessage.Read.All` (administrator consent), `ChannelMessage.Send` |
| File attachments and presence | `Files.ReadWrite.All`, `Presence.ReadWrite` |
| Edit your own channel posts | `ChannelMessage.ReadWrite` (administrator consent), plus a Teams messaging policy allowing edits |

The first five rows are the default requested scopes. To enable channel edits or request a smaller feature set, set the `scopes` array in the Teams CLI `config.toml` to the full list your organization approved and sign in again. The array replaces the default list. [Graph permission descriptions](https://learn.microsoft.com/en-us/graph/permissions-reference) · [Channel and chat message updates](https://learn.microsoft.com/en-us/graph/api/chatmessage-update?view=graph-rest-1.0)

## Sign in and verify

```sh
teams login --client-id YOUR_CLIENT_ID --tenant-id YOUR_TENANT_ID
teams whoami
teams doctor --probe
```

For SSH, add `--device-code` to the login command. To use the same registration for subsequent sign-ins, set `TEAMS_CLIENT_ID` and `TEAMS_TENANT_ID`. Match the sign-in event’s **Application ID** with your registered client ID in **Entra ID → Monitoring & health → Sign-in logs**. Review consent under the corresponding Enterprise application.

Without an override, Teams uses the public **Microsoft Graph Command Line Tools** client. The optional Azure CLI broker uses the identity already signed in to Azure CLI. Your tenant’s policy and the scopes on its issued token determine which Graph operations succeed. [Microsoft application IDs](https://learn.microsoft.com/en-us/troubleshoot/azure/entra/entra-id/governance/verify-first-party-apps-sign-in) · [Azure CLI Graph tokens](https://learn.microsoft.com/en-us/cli/azure/account#az-account-get-access-token)
