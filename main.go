package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"os"
	"os/signal"
	"syscall"
	"time"

	mqtt "github.com/eclipse/paho.mqtt.golang"
	"github.com/leonlatsch/pc2mqtt/internal/appconfig"
	"github.com/leonlatsch/pc2mqtt/internal/entities"
)

var (
	connectionLost        = make(chan struct{}, 1)
	connectionEstablished = make(chan struct{}, 1)
	initialConnectionDone = false
	client                mqtt.Client
)

func main() {
	log.SetFlags(0)

	log.Println("Starting application")

	if err := appconfig.LoadConfig(); err != nil {
		log.Fatalln(err)
	}

	// Setup signal handling for graceful shutdown
	sigChan := make(chan os.Signal, 1)
	signal.Notify(sigChan, os.Interrupt, syscall.SIGTERM, syscall.SIGINT)

	client = createClient()

	// Create a context that can be canceled by signals
	mainCtx, mainCancel := context.WithCancel(context.Background())
	defer mainCancel()

	// Handle OS signals in background
	go func() {
		sig := <-sigChan
		log.Printf("Received signal %v, shutting down gracefully...", sig)
		publishOfflineStatus(client)
		client.Disconnect(2000) // 2 second timeout
		mainCancel()
		os.Exit(0)
	}()

	// Connect to MQTT broker
	if token := client.Connect(); token.Wait() && token.Error() != nil {
		log.Fatalf("Failed to connect to MQTT broker: %v", token.Error())
	}

	// Wait for initial connection
	select {
	case <-connectionEstablished:
		log.Println("Initial connection established")
	case <-time.After(10 * time.Second):
		log.Fatal("Timeout waiting for initial MQTT connection")
	}

	entityList := entities.GetEntities()
	entitiesWithCommands := entities.FilterEntitiesWithCommands(entityList)
	log.Printf("Loaded %d entities (%d with commands)", len(entityList), len(entitiesWithCommands))

	// Wait for shutdown signal
	<-mainCtx.Done()
	log.Println("Application shutting down...")
}

func publishAutoDiscoveryConfigs(client mqtt.Client, entityList []entities.Entity) {
	log.Printf("Publishing auto-discovery configs for %d entities...", len(entityList))
	for i, ety := range entityList {
		configJson, err := json.Marshal(ety.GetDiscoveryConfig())
		if err != nil {
			log.Printf("Error marshaling discovery config for entity %d: %v", i, err)
			continue
		}

		topic := ety.GetDiscoveryTopic()
		token := client.Publish(topic, 1, true, configJson)
		if token.Wait() && token.Error() != nil {
			log.Printf("Error publishing discovery config to %q: %v", topic, token.Error())
			continue
		}
		debugLog(fmt.Sprintf("Published discovery config to %q", topic))
	}

	log.Println("Auto-discovery configs published successfully")
}

func publishAvailability(client mqtt.Client, entityList []entities.Entity) {
	log.Printf("Publishing availability for %d entities...", len(entityList))
	for _, ety := range entityList {
		availability := ety.GetDiscoveryConfig().Availability
		payload := availability.PayloadAvailable
		token := client.Publish(availability.Topic, 1, true, payload)
		if token.Wait() && token.Error() != nil {
			log.Printf("Error publishing availability to %q: %v", availability.Topic, token.Error())
			continue
		}
		debugLog(fmt.Sprintf("Published availability to %q", availability.Topic))
	}

	log.Println("Availability messages published successfully")
}

func publishSensorStates(client mqtt.Client, entityList []entities.Entity) {
	var sensors []entities.BinarySensor
	for _, entity := range entityList {
		switch v := entity.(type) {
		case entities.BinarySensor:
			sensors = append(sensors, v)
		}
	}

	if len(sensors) == 0 {
		debugLog("No binary sensors to publish")
		return
	}

	log.Printf("Publishing states for %d binary sensors...", len(sensors))
	for _, sensor := range sensors {
		topic := sensor.GetDiscoveryConfig().StateTopic
		payload := sensor.DiscoveryConfig.PayloadOn
		token := client.Publish(topic, 1, true, payload)
		if token.Wait() && token.Error() != nil {
			log.Printf("Error publishing sensor state to %q: %v", topic, token.Error())
			continue
		}
		debugLog(fmt.Sprintf("Published sensor state to %q", topic))
	}

	log.Println("Sensor states published successfully")
}

func subscribeToCommandTopics(client mqtt.Client, entitiesWithCommands []entities.EntityWithCommand) {
	if len(entitiesWithCommands) == 0 {
		log.Println("No command topics to subscribe to")
		return
	}

	log.Printf("Will subscribe to %d command topic(s)", len(entitiesWithCommands))

	// Create a message handler
	var messageCount int
	handler := func(client mqtt.Client, msg mqtt.Message) {
		messageCount++
		topic := msg.Topic()
		payload := string(msg.Payload())

		log.Printf("Received message #%d on topic %q: %q", messageCount, topic, payload)

		matched := false
		for _, entity := range entitiesWithCommands {
			if entity.GetDiscoveryConfig().CommandTopic == topic {
				matched = true
				log.Printf("Executing command for topic %q", topic)
				entity.QueueAction()
				break
			}
		}

		if !matched {
			log.Printf("Warning: Received message on unhandled topic %q", topic)
		}
	}

	// Subscribe to all command topics
	filters := make(map[string]byte)
	for _, ety := range entitiesWithCommands {
		topic := ety.GetDiscoveryConfig().CommandTopic
		filters[topic] = 1 // QoS 1
		debugLog(fmt.Sprintf("Subscribing to topic: %s", topic))
	}

	token := client.SubscribeMultiple(filters, handler)
	if token.Wait() && token.Error() != nil {
		log.Printf("Failed to subscribe to command topics: %v", token.Error())
		return
	}

	log.Println("✓ Ready to receive commands")
	debugLog(fmt.Sprintf("Successfully subscribed to %d topics", len(entitiesWithCommands)))
}

func createClient() mqtt.Client {
	appConf := appconfig.RequireConfig()
	clientId := "pc2mqtt-" + appConf.DeviceName
	broker := fmt.Sprintf("tcp://%v:%v", appConf.Mqtt.Host, appConf.Mqtt.Port)

	log.Printf("Creating MQTT client with ID %q for broker %q", clientId, broker)

	opts := mqtt.NewClientOptions()
	opts.AddBroker(broker)
	opts.SetClientID(clientId)
	opts.SetUsername(appConf.Mqtt.Username)
	opts.SetPassword(appConf.Mqtt.Password)
	opts.SetCleanSession(true)
	opts.SetAutoReconnect(true)
	opts.SetConnectRetry(true)
	opts.SetConnectRetryInterval(5 * time.Second)
	opts.SetMaxReconnectInterval(5 * time.Second)
	opts.SetKeepAlive(60 * time.Second)
	opts.SetPingTimeout(10 * time.Second)

	// Set Last Will and Testament
	availability := entities.GetDeviceAvailability()
	opts.SetWill(availability.Topic, availability.PayloadNotAvailable, 1, true)

	// Connection callback
	opts.SetOnConnectHandler(func(client mqtt.Client) {
		appConf := appconfig.RequireConfig()
		log.Printf("Connected to %q:%d", appConf.Mqtt.Host, appConf.Mqtt.Port)

		// Signal connection established
		select {
		case connectionEstablished <- struct{}{}:
		default:
		}

		// Publish configuration and subscribe (on both initial and reconnection)
		go func() {
			entityList := entities.GetEntities()
			entitiesWithCommands := entities.FilterEntitiesWithCommands(entityList)

			if !initialConnectionDone {
				// Only publish auto-discovery configs on initial connection
				publishAutoDiscoveryConfigs(client, entityList)
				initialConnectionDone = true
			}

			publishAvailability(client, entityList)
			publishSensorStates(client, entityList)
			subscribeToCommandTopics(client, entitiesWithCommands)
		}()
	})

	// Connection lost callback
	opts.SetConnectionLostHandler(func(client mqtt.Client, err error) {
		log.Printf("⚠ Connection lost: %v", err)

		select {
		case connectionLost <- struct{}{}:
		default:
		}
	})

	// Reconnecting callback
	opts.SetReconnectingHandler(func(client mqtt.Client, opts *mqtt.ClientOptions) {
		log.Println("Attempting to reconnect to MQTT broker...")
	})

	client := mqtt.NewClient(opts)
	log.Println("MQTT client created successfully")
	return client
}

func debugLog(message string) {
	if appconfig.RequireConfig().DebugMode {
		log.Println(message)
	}
}

func publishOfflineStatus(client mqtt.Client) {
	log.Println("Publishing offline status before shutdown...")
	availability := entities.GetDeviceAvailability()
	payload := availability.PayloadNotAvailable

	token := client.Publish(availability.Topic, 1, true, payload)
	if token.WaitTimeout(2*time.Second) && token.Error() != nil {
		log.Printf("Failed to publish offline status: %v", token.Error())
	} else {
		log.Println("Offline status published successfully")
	}

	// Give the broker time to process
	time.Sleep(500 * time.Millisecond)
}
