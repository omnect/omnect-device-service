<#
.SYNOPSIS
    This script will create and populate a example module for each device in the given IoT hub.
    In case a module already exists for a device, it isn't changed.
    The script is based on microsoft azure powershell script: https://github.com/Azure/Azure-IoT-Security/tree/master/security_module_twin/create_security_module

.DESCRIPTION
    The module will include a empty module twin configuration.

#>

#######################################################################################################################################
# SCRIPT PARAMETERS
#######################################################################################################################################


#######################################################################################################################################
# CONSTANTS
#######################################################################################################################################
# IOT_HUB_CONNECTION_STRING: add your specific IoT Hub connection string
$IOT_HUB_CONNECTION_STRING = "HostName=adu-preview.azure-devices.net;SharedAccessKeyName=iothubowner;SharedAccessKey=llDEc/EYlvCIL7iirQ3J+7Xh+19jaCgoFbO+1BrXa4Y="
# this name will be used for the moduleId 
$MODULE_NAME = "twin-loopback-example"

#######################################################################################################################################
# HELPER FUNCTIONS
#######################################################################################################################################
function CreateConfigurationObject($value) {
    return @{
        "value" = $value
    };
}

function ParseConnectionString($connectionString) {
    if (!($connectionString -match "HostName=([^;]*);SharedAccessKeyName=([^;]*);SharedAccessKey=([^;]*)")) {
        throw "Invalid connection string '$connectionString'. Please enter a connection string in the format of 'HostName=[HostName];SharedAccessKeyName=[SharedAccessKeyName];SharedAccessKey=[SharedAccessKey]'"
    }

    return @{
                hubUri = $Matches[1]
                policyName = $Matches[2]
                key = $Matches[3]
            }
}

function CreateToken($iotHubConnectionParameters) {
    [System.TimeSpan]$fromEpochStart = [System.DateTime]::UtcNow - (New-Object System.DateTime -ArgumentList 1970, 1, 1)
    $expiry = [System.Convert]::ToString([int]$fromEpochStart.TotalSeconds + (60 * 60 * 10))
    $stringToSign = [System.Net.WebUtility]::UrlEncode($iotHubConnectionParameters.hubUri) + "`n" + $expiry
    $hmac = New-Object System.Security.Cryptography.HMACSHA256 -ArgumentList @(,([System.Convert]::FromBase64String($iotHubConnectionParameters.key)))
    $signature = [System.Convert]::ToBase64String($hmac.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($stringToSign)))
    $token = [System.String]::Format([System.Globalization.CultureInfo]::InvariantCulture, "SharedAccessSignature sr={0}&sig={1}&se={2}&skn={3}", [System.Net.WebUtility]::UrlEncode($iotHubConnectionParameters.hubUri), [System.Net.WebUtility]::UrlEncode($signature), $expiry, $iotHubConnectionParameters.policyName)

    return $token
}

function CreateQuery($queryString, $token, $hubUri) {
    return @{ queryString = $queryString; continuationToken = ""; token = $token; hubUri = $hubUri; hasMoreResults = $true }
}

function Query_GetMoreResults($query) {
    $body = "{""query"": ""$($query.queryString)"" }"
    $headers = @{"Authorization" = $query.token; "x-ms-continuation" = $query.continuationToken}
    
    try {
        $response = Invoke-WebRequest -Method Post -Headers $headers -ContentType "application/json; charset=utf-8" -Uri "https://$($query.hubUri)/devices/query?api-version=2018-06-30" -Body $body
        $query.continuationToken = $response.Headers["x-ms-continuation"]
        if ($query.continuationToken -eq "" -or $query.continuationToken -eq $null) {
            $query.hasMoreResults = $false
        }

        return $response.Content
    }
    catch {
        throw "Got an error while trying to execute query. Message:$($_.Exception.Message)"
    }
}

function CreateAndPopulateModuleForDevice($deviceId, $hubUri, $token) {
    # Create the module
    $requestJson = @{
        deviceId = $deviceId
        moduleId = $MODULE_NAME
    }
    $body = ConvertTo-Json -Depth 10 $requestJson
    $headers = @{"Authorization" = $token}
    try {
        Invoke-WebRequest -Method Put -Headers $headers -ContentType "application/json; charset=utf-8" -Uri "https://$hubUri/devices/$deviceId/modules/$($MODULE_NAME)?api-version=2018-06-30" -Body $body
    }
    catch {
        if ($_.Exception.Response.StatusCode -ne 409) {
            Write-Error "Got an error while trying to create a module for device '$deviceId'. Message:$($_.Exception.Message)" 
            return $false
        }
    }
    
    # Populate the module
    # you can add specific tags and desired properties 
    $requestJson = @{
        deviceId = $deviceId
        moduleId = $MODULE_NAME
        tags = @{
        }
        properties = @{
            desired = @{
            }    
        }
    }

    $body = ConvertTo-Json -Depth 10 $requestJson
    $headers = @{"Authorization" = $token}
    try {
        Invoke-WebRequest -Method Patch -Headers $headers -ContentType "application/json; charset=utf-8" -Uri "https://$hubUri/twins/$deviceId/modules/$($MODULE_NAME)?api-version=2018-06-30" -Body $body
    }
    catch {
        Write-Error "Got an error while trying to populate a module for device '$deviceId'. Message: $($_.Exception.Message)" 
        return $false
    }   

    return $true
}

function GetDevicesWithModule($hubUri, $token) {
    $res = @()
    $queryString = "SELECT * FROM devices.modules WHERE moduleId = '$MODULE_NAME'"
    $query = CreateQuery -queryString $queryString -token $token -hubUri $hubUri
    do {
        $queryResult = Query_GetMoreResults -query $query
        $res += (ForEach-Object -InputObject (ConvertFrom-Json $queryResult) {$_.DeviceId})
    } while ($query.hasMoreResults)

    return ,$res 
}

#######################################################################################################################################
# MAIN
#######################################################################################################################################
$iotHubConnectionParameters = ParseConnectionString -connectionString $IOT_HUB_CONNECTION_STRING
$token = CreateToken -iotHubConnectionParameters $iotHubConnectionParameters
$devicesWithModule = GetDevicesWithModule -hubUri $iotHubConnectionParameters.hubUri -token $token

$queryString = "SELECT * FROM devices"
$query = CreateQuery -queryString $queryString -token $token -hubUri $iotHubConnectionParameters.hubUri

$successfulModuleCreateOperations = 0
$unSuccessfulModuleCreateOperations = 0

do {
    $queryResult = Query_GetMoreResults -query $query
    $devices = (ForEach-Object -InputObject (ConvertFrom-Json $queryResult) {$_.DeviceId})
    $devicesWithoutModule = $devices | where {!($devicesWithModule.Contains($_))}
    foreach ($device in $devicesWithoutModule) {
        if (CreateAndPopulateModuleForDevice -deviceId $device -hubUri $iotHubConnectionParameters.hubUri -token $token) {
            $successfulModuleCreateOperations += 1
        }
        else {
            $unSuccessfulModuleCreateOperations += 1
        }
        Write-Host "create and populate a module for deviceId: $($device)"
    }
} while ($query.hasMoreResults)

Write-Host "Statistics:"
Write-Host "Number of devices that already have a module: $($devicesWithModule.Count)"
Write-Host "Number of devices that haven't a module: $($devicesWithoutModule.Count)"
Write-Host "Number of modules that was created successfully: $successfulModuleCreateOperations"
Write-Host "Number of modules that wasn't created successfully: $unSuccessfulModuleCreateOperations"