/**
 * Connects the app to the internal "hosts API" source: logs in once,
 * syncs machines into a single-choice folder, and stores the password
 * only in the OS keychain (never in config.json or the manifest). See
 * src-tauri/src/hosts_api.rs and hosts_api_sync.rs for the backend half.
 */

import { Box, Button, Group, PasswordInput, Text, TextInput } from '@mantine/core'
import events from '@common/events'
import SideDrawer from '@renderer/components/SideDrawer'
import { actions } from '@renderer/core/agent'
import { getErrorMessage, showErrorNotification, showSuccessNotification } from '@renderer/core/notify'
import useOnBroadcast from '@renderer/core/useOnBroadcast'
import useConfigs from '@renderer/models/useConfigs'
import useI18n from '@renderer/models/useI18n'
import { useState } from 'react'
import { BiServer } from 'react-icons/bi'

const AddHostsApiSource = () => {
  const { lang, i18n } = useI18n()
  const { configs, updateConfigs } = useConfigs()
  const [isShow, setIsShow] = useState(false)
  const [endpoint, setEndpoint] = useState('')
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [isConnecting, setIsConnecting] = useState(false)

  useOnBroadcast(
    events.add_hosts_api_source,
    () => {
      setEndpoint(configs?.hosts_api_base_url || '')
      setUsername(configs?.hosts_api_username || '')
      setPassword('')
      setIsShow(true)
    },
    [configs],
  )

  const onClose = () => {
    setPassword('')
    setIsShow(false)
  }

  const onConnect = async () => {
    const trimmedEndpoint = endpoint.trim().replace(/\/+$/, '')
    const trimmedUsername = username.trim()
    if (!trimmedEndpoint || !trimmedUsername || !password) return

    setIsConnecting(true)
    try {
      await updateConfigs(
        {
          hosts_api_base_url: trimmedEndpoint,
          hosts_api_username: trimmedUsername,
        },
        { notifyError: false },
      )
      await actions.hostsApiSavePassword(trimmedUsername, password)
      const result = await actions.syncHostsApi()
      if (result?.success) {
        showSuccessNotification({
          title: lang.hosts_api_connect,
          message: i18n.trans('hosts_api_synced', [String(result.machines_synced ?? 0)]),
        })
        setPassword('')
        setIsShow(false)
      } else {
        showErrorNotification({
          title: lang.hosts_api_connect,
          message: result?.message || lang.fail,
        })
      }
    } catch (e) {
      showErrorNotification({
        title: lang.hosts_api_connect,
        message: getErrorMessage(e, lang.fail),
      })
    } finally {
      setIsConnecting(false)
    }
  }

  return (
    <SideDrawer
      opened={isShow}
      onClose={onClose}
      title={
        <Group gap="8px">
          <BiServer />
          <Box>{lang.hosts_api_add_title}</Box>
        </Group>
      }
      footer={
        <Group justify="flex-end" gap="12px" style={{ width: '100%' }}>
          <Button onClick={onClose} variant="outline">
            {lang.btn_cancel}
          </Button>
          <Button onClick={onConnect} loading={isConnecting}>
            {lang.hosts_api_connect}
          </Button>
        </Group>
      }
    >
      <Box p="16px">
        <Box mb="16px">
          <Text mb="8px">{lang.hosts_api_endpoint}</Text>
          <TextInput
            aria-label={lang.hosts_api_endpoint}
            data-autofocus
            value={endpoint}
            placeholder="https://"
            onChange={(e) => setEndpoint(e.target.value)}
          />
        </Box>
        <Box mb="16px">
          <Text mb="8px">{lang.hosts_api_username}</Text>
          <TextInput
            aria-label={lang.hosts_api_username}
            value={username}
            onChange={(e) => setUsername(e.target.value)}
          />
        </Box>
        <Box mb="16px">
          <Text mb="8px">{lang.password}</Text>
          <PasswordInput
            aria-label={lang.password}
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && !isConnecting && onConnect()}
          />
        </Box>
      </Box>
    </SideDrawer>
  )
}

export default AddHostsApiSource
